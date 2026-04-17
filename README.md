# brokkr

Command orchestrator and development utility for [pbfhogg](https://github.com/folknor/pbfhogg), [elivagar](https://github.com/folknor/elivagar), [nidhogg](https://github.com/folknor/nidhogg), [litehtml-rs](https://github.com/folknor/litehtml-rs), and [sluggrs](https://github.com/folknor/sluggrs). Single Rust binary that provides benchmarking, verification, profiling, visual reference testing, and operational commands across all projects.

Built with LLMs. See [LLM.md](LLM.md).


## Install

```
cargo install --path ~/Programs/brokkr
```

## How it works

Run `brokkr` from any project root. It reads `./brokkr.toml` to detect which project you're in and resolves datasets, paths, and host-specific configuration. Commands are project-gated — running a pbfhogg command from elivagar's root produces a clear error.

```
cd ~/Programs/pbfhogg
brokkr inspect --tags --dataset denmark             # run once, print timing
brokkr inspect --tags --dataset denmark --bench     # 3 runs, store in DB
brokkr inspect --tags --dataset denmark --hotpath   # function-level timing
brokkr verify sort                                  # cross-validate against osmium

cd ~/Programs/elivagar
brokkr tilegen --dataset denmark --bench            # full pipeline benchmark
brokkr pmtiles-writer --hotpath                     # micro-benchmark hotpath

cd ~/Programs/nidhogg
brokkr serve                                       # start the nidhogg server
brokkr api --dataset denmark --bench                # API query benchmark

cd ~/Programs/litehtml-rs
brokkr test --all                                   # visual reference tests
```

## Commands

### Measurement modes

Every measurable command supports these flags:

| Flag | Behavior |
|------|----------|
| *(none)* | Build, run once, print timing. No DB storage. |
| `--bench` | Full benchmark: lockfile, 3 runs, best-of-N stored in DB |
| `--bench N` | Same but N runs |
| `--hotpath` | Function-level timing via hotpath feature (1 run) |
| `--hotpath N` | Same but N runs |
| `--alloc` | Per-function allocation tracking (1 run) |
| `--stop <marker>` | Kill the child when this FIFO marker is emitted (bench any phase in isolation) |

All measured modes automatically attach a sidecar that samples `/proc` metrics at 100ms (see [Sidecar profiler](#sidecar-profiler) below).

All commands also accept `--dataset`, `--variant`, `--commit`, `--features`, `--force`, `--verbose`, `--wait`.

pbfhogg commands additionally accept `--direct-io` and `--io-uring` to enable O_DIRECT and io_uring I/O paths. These add the required cargo features to the build, pass the flags to the binary, and show up in the `cli_args`/`brokkr_args` columns of the results DB (query via `brokkr results --grep direct-io`). `--direct-io` works with all commands. `--io-uring` is only supported by `apply-changes`, `sort`, `cat --dedupe`, and `diff --format osc` — brokkr rejects it for other commands before building. io_uring preflight checks run automatically.

### Shared (all projects)

| Command | Description |
|---------|-------------|
| `check` | Run clippy + tests (extra args forwarded to `cargo test`) |
| `env` | Show hostname, kernel, governor, memory, drives, tool versions, dataset status |
| `results` | Query the results database (`.brokkr/results.db`) |
| `clean` | Remove scratch/temp files |
| `pmtiles-stats` | PMTiles v3 file statistics |
| `history` | Browse global command history |
| `preview` | Run full pipeline (enrich → tilegen → ingest → serve) and open map viewer |
| `lock` | Show who holds the benchmark lock |

`check` filters cargo output into one line per diagnostic. Compilation noise is stripped; each error or warning becomes `error[CODE] file:line:col message` or `warning[rule] file:line:col message`. Passing tests are aggregated (e.g. `cargo test: 137 passed (4 suites, 1.45s)`), failures become `FAILED name location message`. Use `--raw` for unfiltered cargo output, or `--json` for NDJSON with full-fidelity structured diagnostics (one JSON object per line). Falls back to raw output automatically if parsing fails.

### pbfhogg

Every pbfhogg CLI command is a top-level brokkr subcommand: `inspect`, `check-refs`, `sort`, `cat`, `add-locations-to-ways`, `build-geocode-index`, `apply-changes`, `merge-changes`, `extract`, `tags-filter`, `getid`, `diff`, etc.

Several commands are flag-driven umbrellas that previously had separate subcommand names:

- `brokkr inspect` — no flag: metadata; `--nodes`: node stats; `--tags`: tag frequencies (narrow with `--type node|way|relation`)
- `brokkr cat` — bare: indexdata-generation passthrough (no re-decode); `--type way|relation`: filtered full-decode; `--dedupe`: two-input dedupe path (supports `--io-uring`); `--clean`: full-decode / re-frame Framed path. Flags are orthogonal and combinable. Bare `cat` defaults to `--variant raw` since that's the natural bootstrap input.
- `brokkr tags-filter` — `--filter EXPR` (default `w/highway=primary`), `-R` for single-pass (drop referenced), `--input-kind osc` to read an OSC diff instead of a PBF
- `brokkr getid` — hardcoded ID set; `--add-referenced` for two-pass, `--invert` to negate
- `brokkr extract` — `--strategy simple|complete|smart` picks the extract algorithm
- `brokkr diff` — `--format default|osc` picks the output shape

`add-locations-to-ways` accepts `--index-type` (dense, sparse, external; default: hash). When using `--index-type external`, two additional flags are available for iterating on individual pipeline stages: `--keep-scratch` preserves the external join's intermediate scratch directory after the run, and `--start-stage N` (2-4) skips stages 1..N-1 by reusing the scratch from a prior `--keep-scratch` run. `--start-stage` implies `--keep-scratch` so subsequent partial runs don't clean up the scratch. All of `--index-type`, `--start-stage`, and `--keep-scratch` land in `cli_args` verbatim, so partial runs don't mix with full-pipeline baselines and can be isolated via `brokkr results --grep 'start-stage' --command add-locations-to-ways`.

Multi-variant benchmarks: `read`, `write`, `merge`, `extract` (with `--strategy`, `--modes`, `--compressions` flags — `--compressions` is plural because the single-value `--compression` collides with pbfhogg's passthrough flag on `write` / `merge`).

`merge-changes` accepts `--osc-seq <N>` for a single OSC file (back-compat) or `--osc-range LO..HI` to merge a contiguous range of configured OSC entries in one invocation. The range form lands in `cli_args` verbatim, so `brokkr results --command merge-changes --grep 'osc-range'` finds the range-form runs.

`brokkr diff` (with or without `--format osc`) derives its second input by running `apply-changes` on the dataset's PBF + OSC and caching the result at `<scratch>/<pbf-stem>-osc<seq>-bench-merged.osm.pbf`. The cache key includes the OSC seq so different `--osc-seq` invocations don't silently reuse each other's merged files. In any measured mode (`--bench`/`--hotpath`/`--alloc`) the cache is rebuilt before the run so total invocation wall time is reproducible; pass `--keep-cache` to opt back into reuse. Run mode (no measurement flag) always reuses the cache for dev-loop speed. Cache hit/miss + age land in the result row's metadata as `meta.merged_cache` and `meta.merged_cache_age_s`.

`diff-snapshots` benchmarks pbfhogg's `diff` against two independent point-in-time snapshots of the same dataset (e.g. `planet-20260223` vs `planet-20260411`). Unlike `diff` (with or without `--format osc`) — which derives its B side from `apply-changes` and therefore preserves blob-level byte equality with the A side — `diff-snapshots` forces every blob through full decode on both sides. Different working set, different peak memory, different wall time. The dataset's primary (legacy top-level) PBF is referenced as `base`; additional snapshots registered via `brokkr download <region> --as-snapshot <key>` are referenced by their snapshot key. The `--format` flag selects between summary diff (default) and OSC-format output. The `--from`/`--to`/`--format` choices are recorded verbatim in `cli_args` — query osc-only runs via `brokkr results --command diff-snapshots --grep 'format osc'`.

```
brokkr diff-snapshots --dataset planet --from base --to 20260411 --bench 1
brokkr diff-snapshots --dataset planet --from 20260411 --to 20260418 --format osc
```

`suite pbfhogg` runs the full benchmark suite.

**Verification** (`brokkr verify <subcommand>`): cross-validates against osmium, osmosis, and osmconvert. Subcommands: `sort`, `cat`, `extract`, `multi-extract`, `tags-filter`, `getid-removeid`, `add-locations-to-ways`, `check-refs`, `merge` (apply-changes), `derive-changes` (diff → osc roundtrip), `renumber`, `diff`, and `all` (runs them all). Verify subcommands keep their own short names; they don't mirror the consolidated CLI umbrella shape.

`verify renumber` is a special case. Most verify commands require pbfhogg's output to be byte-identical (or element-identical) with osmium's. `renumber` does not: pbfhogg's orphan-reference handling in relation members is a documented intentional deviation (see pbfhogg's `DEVIATIONS.md` and `notes/renumber-planet-scale.md` section 5b), so a small non-zero diff is expected and does not indicate a regression. The goal of the command is to separate "expected delta" from "actual regression" without a human having to triage every diff.

```
brokkr verify renumber                              # default: denmark
brokkr verify renumber --dataset europe --verbose   # print detail on mismatch
brokkr verify renumber --start-id 1,1,1             # forwarded to both tools
```

Per run it renumbers the input PBF with both tools, runs `pbfhogg diff -s -c -v` on the two outputs, and classifies the result:

1. Parses the `Summary: left=N right=M same=X different=Y` line to get element counts.
2. Scans the detail output for `*n<id>` / `*w<id>` / `*r<id>` block headers and counts diff blocks per element type.
3. Runs `pbfhogg inspect` on the osmium output to recover the total relation count for the threshold check.

**PASS** when element counts match, no node or way block headers appear in the diff, and the total diff count stays under `0.10 * total_relations` (sanity threshold — calibrated from measured rates like Denmark's 306 orphan-ref diffs ÷ 46,103 relations ≈ 0.66%; the threshold catches regressions that would typically be orders of magnitude higher without flagging normal transboundary delta).

**FAIL** when any of those three checks fire: divergent element counts, any node/way diff, or relation diffs that blow past the sanity threshold. On failure the diff log at `target/verify/renumber/verify-renumber-<dataset>-diff.txt` is preserved alongside both renumbered PBFs for human review. On success all three scratch files are removed. The `--verbose` flag additionally prints the first 50 lines of the diff to the terminal when any mismatch (expected or not) is found. `verify all` includes `renumber` as part of the pre-release sweep.

**Other**: `download <region> [--osc-seq N]` fetches datasets from Geofabrik. Accepts short aliases (`denmark`, `europe`) or full Geofabrik paths (`europe/france`, `asia/japan/kanto`). Skips files that already exist (checked against `brokkr.toml` filenames). `--osc-seq N` downloads all missing OSC diffs from the last configured seq through N, hashes them, and appends entries to `brokkr.toml`. New downloads use dated filenames matching the project convention (e.g. `europe-20260329-seq4716.osc.gz`).

`download <region> --as-snapshot <key>` registers a new historical snapshot of an existing dataset under `[host.datasets.<region>.snapshot.<key>]` instead of touching the dataset's primary pbf/osc tables. Requires the dataset to already exist (run `brokkr download <region>` first). The snapshot key must match `[a-zA-Z0-9_-]+`; `base` is reserved as the CLI sentinel for the dataset's legacy/primary data. Files are written with snapshot-specific names (`<region>-<key>.osm.pbf`, etc.) and the indexed PBF is generated automatically.

`download <region> --refresh` rotates the dataset to a newer upstream snapshot. HEAD-checks upstream `Last-Modified` first; no-ops if not newer than the existing pbf.raw's mtime / `download_date` (use `--force` to rotate anyway). On rotation: archives the existing primary pbf/osc tables under a `[snapshot.<key>]` block (key derived from `download_date` or file mtime as `YYYYMMDD`), downloads the new PBF, generates the indexed PBF via `pbfhogg cat`, updates `download_date` to today, and resets the OSC chain. Errors with a clear message if the derived snapshot key collides with an existing snapshot block. After refresh, the archived state is reachable via `brokkr diff-snapshots --from <key> --to base` and `brokkr apply-changes --dataset <region> --snapshot <key> --osc-seq <N>`.

`apply-changes`, `merge-changes`, `tags-filter --input-kind osc`, and `diff` (including `--format osc`) all accept `--snapshot <key>` to read PBF and/or OSC data from a historical snapshot rather than the dataset's primary tables. `--snapshot base` (or omitting the flag) preserves the existing behavior — script-friendly when parameterizing over snapshot keys. The `--snapshot` flag lands verbatim in `cli_args`, so `brokkr results --grep 'snapshot 20260411'` (or just `--grep 20260411`) finds every command run against that snapshot.

Calling plain `brokkr download <region>` against a dataset whose `pbf.raw` is already configured is a SKIP (no auto-refresh), and prints a multi-line message naming both `--refresh` and `--as-snapshot` so the user knows the alternatives without having to read the source.

### elivagar

| Command | Description |
|---------|-------------|
| `tilegen` | Full tile generation pipeline (with all pipeline flags) |
| `pmtiles-writer` | PMTiles writer micro-benchmark (`--tiles N`) |
| `node-store` | SortedNodeStore micro-benchmark (`--nodes N`) |
| `planetiler` | Planetiler comparison |
| `tilemaker` | Tilemaker comparison |

`suite elivagar` runs the full benchmark suite.

**Other**: `compare-tiles`, `download-ocean`, `download-natural-earth`.

### nidhogg

**Server**: `serve`, `stop`, `status`.

**Operations**: `ingest`, `update`, `query`, `geocode`.

**Benchmarks**: `api` (query performance), `nid-ingest` (ingest), `tiles` (tile serving).

**Verification**: `batch`, `nid-geocode`, `readonly`.

### litehtml-rs

Visual reference testing and fixture preprocessing. All commands are top-level (no `brokkr litehtml` namespace). Shared visual testing commands (`test`, `list`, `approve`, `report`, `visual-status`) dispatch to litehtml or sluggrs based on the detected project.

| Subcommand | Description |
|------------|-------------|
| `test` | Run fixtures against Chrome reference artifacts (pixel diff + element comparison) |
| `list` | Show fixtures, tags, and approval state |
| `approve` | Record current divergence as accepted baseline |
| `visual-status` | Dashboard of all fixtures vs approved baselines |
| `report` | Show results for a past test run |
| `prepare` | Normalize raw email HTML into self-contained fixture (images → gray PNGs, inject Ahem font, strip external resources, pretty-print) |
| `html-extract` | Extract sub-fixture by CSS selector (`--selector`) or sibling range (`--from`/`--to`) |
| `outline` | Structural overview with section markers, content previews, and suggested selectors |

**Fixture workflow**:
```
brokkr prepare raw-email.html fixtures/email-prepared.html
brokkr outline fixtures/email-prepared.html --selectors
brokkr html-extract fixtures/email-prepared.html \
  --from "div:nth-of-type(2) > table > tbody > tr > td > div:nth-of-type(4) > div" \
  --to   "div:nth-of-type(2) > table > tbody > tr > td > div:nth-of-type(7) > div" \
  fixtures/creatine_products.html
brokkr test creatine_products
```

`prepare` and `html-extract` shell out to a Node.js script (requires Node + pnpm; auto-installs dependencies on first use). Node is already required for Puppeteer-based Chrome capture.

## Preview pipeline

`brokkr preview` runs the full data pipeline and opens a map viewer for visual inspection:

```
brokkr preview                          # full pipeline, default dataset/variant
brokkr preview --from tilegen           # skip enrich, start from tile generation
brokkr preview --from serve --no-open   # just restart server, don't open browser
brokkr preview --dataset japan --variant raw
```

Steps: **enrich** (pbfhogg add-locations-to-ways) → **tilegen** (elivagar run) → **ingest** (nidhogg ingest) → **serve** (nidhogg serve + browser). Use `--from` to skip upstream steps when iterating on a single project.

Requires a `[hostname.preview]` section in `brokkr.toml` pointing to each project's source tree:

```toml
[plantasjen.preview]
pbfhogg = "/home/folk/Programs/pbfhogg"
elivagar = "/home/folk/Programs/elivagar"
nidhogg = "/home/folk/Programs/nidhogg"
```

Artifacts are written to `.brokkr/preview/` (enriched PBF, PMTiles, ingest data dir). Works from any of the three project roots.

## Benchmark harness

All benchmarks run through `BenchHarness`, which provides:

- **Exclusive lock** — prevents parallel bench/verify/hotpath runs via lockfile
- **SQLite storage** — results stored in `.brokkr/results.db` per project with git commit, hostname, and full environment snapshot
- **Multiple timing modes** — in-process (N runs, best-of-N), subprocess (external binary), and distribution (min/p50/p95/max)
- **Retroactive benchmarking** — `--commit <hash>` builds and benchmarks old commits via git worktree
- **OOM protection** — memory availability checks before large-scale runs

## Sidecar profiler

Every measured run (bench, hotpath, alloc) automatically samples `/proc/{pid}/stat`, `/proc/{pid}/io`, and `/proc/{pid}/status` at 100ms intervals. Data is stored in `.brokkr/sidecar.db` (gitignored — local to the machine that ran it). The main results in `.brokkr/results.db` stay small and git-tracked.

The child process receives `BROKKR_MARKER_FIFO` env var pointing to a named pipe for application phase markers and counters. Markers are lines of the form `<timestamp_us> <name>`, counters are `<timestamp_us> @<name>=<value>`. Markers are point-in-time bookmarks — the protocol has no notion of spans or pairs. `--markers --durations` derives pair durations from a `FOO_START` / `FOO_END` convention if the emitter happens to follow it, but nothing else assumes that structure.

`--stop <marker>` kills the child process as soon as the named marker is emitted, allowing benchmarks of individual phases without waiting for the full run to complete. The SIGKILL exit is treated as success.

Sidecar data is stored even when the child is OOM-killed — the `/proc` trajectory up to the kill is the most valuable use case.

### Querying sidecar data

A UUID prefix is required (except for `--compare`) — use `brokkr results` to find one. The `dirty` pseudo-UUID resolves to the most recent failed/dirty-tree run.

```
brokkr sidecar <uuid>                              # per-phase summary (default view)
brokkr sidecar <uuid> --human                      # same, as a fixed-width table
brokkr sidecar <uuid> --samples                    # raw /proc samples (JSONL)
brokkr sidecar <uuid> --samples --fields rss,anon --every 10  # project + downsample
brokkr sidecar <uuid> --samples --where "majflt>0" --tail 20  # filter + range
brokkr sidecar <uuid> --samples --phase STAGE2                # filter to a marker phase
brokkr sidecar <uuid> --samples --range 10.0..82.0            # time window filter
brokkr sidecar <uuid> --markers                    # raw marker events (JSONL)
brokkr sidecar <uuid> --durations                  # START/END pair timings
brokkr sidecar <uuid> --counters                   # application counters
brokkr sidecar <uuid> --stat anon                  # min/max/avg/p50/p95 for a field
brokkr sidecar <uuid> --stat anon --phase STAGE2   # per-phase stat
brokkr sidecar --compare <uuid_a> <uuid_b>         # phase-aligned comparison
brokkr sidecar dirty --stat anon                   # inspect last failed/dirty run
```

Filter flags (`--phase`, `--range`, `--where`) compose with `--samples` and `--stat`.

## Results database

Query stored benchmarks with `brokkr results`:

```
brokkr results                                      # table of last 20 results
brokkr results 0b74fb6f                             # look up by UUID prefix
brokkr results --command read                       # last 20 matching 'read'
brokkr results --commit a65a                        # filter by commit
brokkr results --mode hotpath                       # filter by measurement mode
brokkr results --grep pipelined                     # substring-match cli_args + brokkr_args
brokkr results --dataset europe                     # filter by dataset (substring on input file)
brokkr results --command tags-filter --dataset eu   # combine filters
brokkr results --meta merged_cache=miss             # filter by runtime-observation metadata key
brokkr results --command diff-snapshots --grep 'format osc'     # osc-only diff-snapshots runs
brokkr results --meta merged_cache=miss --command diff           # cold-cache diff runs only
brokkr results --compare a65a 911c                  # compare two commits side-by-side
brokkr results --compare-last                       # compare two most recent commits
brokkr results --compare-last --mode hotpath        # compare two most recent hotpath runs
```

Columns that drive the filters:

- `command` — the bare subcommand id (`read`, `cat`, `diff-snapshots`, `api`, …); no `bench `/`hotpath ` prefix.
- `mode` — the measurement mode only (`bench` / `hotpath` / `alloc`).
- `cli_args` — literal subprocess argv (pbfhogg/elivagar/nidhogg). Every flag and axis the user passed is here verbatim.
- `brokkr_args` — literal `brokkr <...>` invocation the user typed. Same-column-dual-view, recorded so `--grep` can find either.
- `meta.*` (via `--meta KEY=VALUE`) — runtime observations only: resolved paths, detected modes, cache hit/miss. Anything derivable from `cli_args` or `brokkr_args` is NOT duplicated here. `meta.` prefix is implicit; multiple `--meta` flags AND together; rows missing the requested key are silently excluded. Available keys depend on the command — e.g. `diff` emits `meta.merged_cache` + `meta.merged_cache_age_s`; elivagar's `tilegen` emits `meta.locations_on_ways_detected`; historical `meta.format`/`meta.index_type`/`meta.start_stage` keys were migrated out in v13 (they live in `cli_args` now).

Use `--grep` for anything that's a flag/axis (`--grep zstd:1`, `--grep 'snapshot 20260411'`, `--grep 'index-type external'`). Use `--meta` for genuine runtime observations.

The `dataset` column in the output table is the first dash-separated component of the input filename — `europe-20260301-seq4714-with-indexdata.osm.pbf` renders as `europe`. This is a display heuristic: filtering via `--dataset` always substring-matches the full `input_file` column, so filters still work even when the short name collapses distinct datasets (e.g. a hypothetical `europe-west` would display as `europe`). The full filename and size are shown in the single-result detail view (`brokkr results <uuid>`) as the `input` field. See TODO.md for the proper fix.

The compare view shows timing, output size, peak RSS, rewrite ratio, and blob distribution columns as applicable. Hotpath comparisons include function-level timing diffs.

## Quick runtime timing

By default, every measurable command builds and runs once with timing output — no DB, no harness overhead:

```
brokkr inspect --tags --dataset denmark
# [run] /path/to/pbfhogg inspect tags denmark.osm.pbf --min-count 999999999
# ... command output ...
# [run] elapsed=1234ms
```

Add `--bench` to enable the full harness with DB storage:

```
brokkr inspect --tags --dataset denmark --bench    # 3 runs, best-of-N stored
brokkr inspect --tags --dataset denmark --bench 10 # 10 runs
```

For ad-hoc passthrough with raw args: `brokkr passthrough -- <args>`.

## Configuration

Each project has a `brokkr.toml` in its root:

```toml
project = "pbfhogg"

# Host-specific config (matched by hostname)
[plantasjen]
data = "data"
scratch = "data/scratch"
target = "target"
port = 3033
drives.source = "nvme"
drives.data = "ssd"
features = ["linux-direct-io", "linux-io-uring"]

[plantasjen.datasets.denmark]
origin = "Geofabrik"
download_date = "2026-02-20"
bbox = "8.0,54.5,13.0,58.0"

[plantasjen.datasets.denmark.pbf.indexed]
file = "denmark-with-indexdata.osm.pbf"
xxhash = "a1b2c3d4e5f6..."
seq = 4704

[plantasjen.datasets.denmark.pbf.raw]
file = "denmark-raw.osm.pbf"
seq = 4704

[plantasjen.datasets.denmark.osc.4705]
file = "denmark-4705.osc.gz"
xxhash = "f1e2d3c4b5a6..."

# Optional historical snapshots — additional point-in-time captures of the
# same dataset, registered via `brokkr download denmark --as-snapshot <key>`.
# The legacy top-level pbf/osc data above is implicitly snapshot `base`.
# `diff-snapshots --from base --to 20260411` diffs the two.
[plantasjen.datasets.denmark.snapshot.20260411]
download_date = "2026-04-11"
seq = 4969

[plantasjen.datasets.denmark.snapshot.20260411.pbf.raw]
file = "denmark-20260411.osm.pbf"
xxhash = "..."

[plantasjen.datasets.denmark.snapshot.20260411.pbf.indexed]
file = "denmark-20260411-with-indexdata.osm.pbf"
xxhash = "..."

# Cross-project source trees for preview pipeline
[plantasjen.preview]
pbfhogg = "/home/folk/Programs/pbfhogg"
elivagar = "/home/folk/Programs/elivagar"
nidhogg = "/home/folk/Programs/nidhogg"
```

- `project` — which project this is (`pbfhogg`, `elivagar`, `nidhogg`, or `litehtml-rs`)
- `[hostname.datasets.*]` — named datasets with PBF variants, OSC diffs, PMTiles entries, and bounding box
- `[hostname.datasets.*.snapshot.<key>]` — additional historical snapshots of the dataset (different point-in-time PBFs of the same region). Each snapshot has its own `pbf` and optional `osc` tables. The legacy top-level data is implicitly snapshot `base` (a reserved name). Snapshot keys must match `[a-zA-Z0-9_-]+`. Snapshots are first-class for `diff-snapshots` and addressable via `--snapshot <key>` on `apply-changes`, `merge-changes`, `tags-filter --input-kind osc`, and `diff` (including `--format osc`). Refresh-mode downloads (`brokkr download <region> --refresh`) populate snapshot blocks automatically by archiving the previous primary state
- `xxhash` — optional XXH128 hash for file integrity checks (`sha256` accepted as alias during migration). Run `brokkr env` to see computed hashes for updating config
- `[hostname]` — per-host path overrides, port, drive configuration, and default cargo features; defaults to `data/`, `data/scratch/`, and cargo target dir
- `features` — cargo features appended to every build (all measurable commands, `verify`, `serve`, `ingest`, `update`). Not applied to `check`. CLI `--features` are additive on top

## License

Apache-2.0
