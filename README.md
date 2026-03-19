# brokkr

Command orchestrator and development utility for [pbfhogg](https://github.com/folkol/pbfhogg), elivagar, nidhogg, and litehtml-rs. Single Rust binary that provides benchmarking, verification, profiling, visual reference testing, and operational commands across all projects.

## Install

```
cargo install --path ~/Programs/brokkr
```

## How it works

Run `brokkr` from any project root. It reads `./brokkr.toml` to detect which project you're in and resolves datasets, paths, and host-specific configuration. Commands are project-gated — running a pbfhogg command from elivagar's root produces a clear error.

```
cd ~/Programs/pbfhogg
brokkr bench read              # pbfhogg read benchmark
brokkr verify sort             # cross-validate sort against osmium

cd ~/Programs/elivagar
brokkr bench self              # elivagar full pipeline benchmark
brokkr hotpath pmtiles         # pmtiles micro-benchmark hotpath

cd ~/Programs/nidhogg
brokkr serve                   # start the nidhogg server
brokkr bench api               # API query benchmark

cd ~/Programs/litehtml-rs
brokkr litehtml test --all     # visual reference tests
brokkr litehtml outline fixtures/prepared.html --selectors
```

## Commands

### Shared (all projects)

| Command | Description |
|---------|-------------|
| `check` | Run clippy + tests (extra args forwarded to `cargo test`). Supports `--features` and `--no-default-features` |
| `env` | Show hostname, kernel, governor, memory, drives, tool versions, dataset status (with XXH128 hashes) |
| `run` | Build (or `--no-build`) and run with passthrough args; supports `--time`, `--json`, `--runs N` |
| `results` | Query the results database (`.brokkr/results.db`) |
| `clean` | Remove scratch/temp files |
| `hotpath` | Function-level timing/allocation profiling via `hotpath` feature |
| `profile` | Sampling profiler (perf/samply) |
| `pmtiles-stats` | PMTiles v3 file statistics (zoom distribution, tile sizes, compression) |
| `preview` | Run full pipeline (enrich → tilegen → ingest → serve) and open map viewer |
| `lock` | Show who holds the benchmark lock |

### pbfhogg

**Benchmarks** (`brokkr bench <subcommand>`):

| Subcommand | Description |
|------------|-------------|
| `read` | Read benchmark (sequential, parallel, pipelined, mmap, blobreader) |
| `write` | Write benchmark (sync + pipelined x compression) |
| `merge` | Merge benchmark (I/O modes x compression) |
| `commands` | CLI commands benchmark (external timing) |
| `extract` | Extract strategies (simple/complete/smart) |
| `allocator` | Allocator comparison (default/jemalloc/mimalloc) |
| `blob-filter` | Indexed vs non-indexed PBF performance |
| `planetiler` | Planetiler Java PBF read comparison |
| `all` | Full benchmark suite |

**Verification** (`brokkr verify <subcommand>`): cross-validates output against osmium, osmosis, and osmconvert for sort, cat, extract, tags-filter, getid/removeid, add-locations-to-ways, check-refs, merge, derive-changes, and diff.

**Other**: `download <region>` fetches datasets from Geofabrik.

### elivagar

**Benchmarks**: `self` (full pipeline), `node-store`, `pmtiles`, `planetiler`, `tilemaker`, `all`.

For `elivagar`-specific node-store behavior, `brokkr bench self`, `brokkr hotpath`, and `brokkr profile` also forward:
- `--force-sorted`
- `--allow-unsafe-flat-index`

**Other**: `compare-tiles` (feature count comparison between PMTiles archives), `download-ocean` (ocean shapefiles).

### nidhogg

**Server**: `serve`, `stop`, `status`.

**Operations**: `ingest` (PBF to disk format), `update` (diff application), `query`, `geocode`.

**Benchmarks**: `api` (query performance), `nid-ingest` (ingest performance).

**Verification**: `batch [--dataset]` (batch query, bbox from dataset config), `nid-geocode`, `readonly [--dataset]` (read-only filesystem).

### litehtml-rs

Visual reference testing and fixture preprocessing (`brokkr litehtml <subcommand>`):

| Subcommand | Description |
|------------|-------------|
| `test` | Run fixtures against Chrome reference artifacts (pixel diff + element comparison) |
| `list` | Show fixtures, tags, and approval state |
| `approve` | Record current divergence as accepted baseline |
| `status` | Dashboard of all fixtures vs approved baselines |
| `report` | Show results for a past test run |
| `prepare` | Normalize raw email HTML into self-contained fixture (images → gray PNGs, inject Ahem font, strip external resources, pretty-print) |
| `extract` | Extract sub-fixture by CSS selector (`--selector`) or sibling range (`--from`/`--to`) |
| `outline` | Structural overview with section markers, content previews, and suggested selectors |

**Fixture workflow**:
```
brokkr litehtml prepare raw-email.html fixtures/email-prepared.html
brokkr litehtml outline fixtures/email-prepared.html --selectors
brokkr litehtml extract fixtures/email-prepared.html \
  --from "div:nth-of-type(2) > table > tbody > tr > td > div:nth-of-type(4) > div" \
  --to   "div:nth-of-type(2) > table > tbody > tr > td > div:nth-of-type(7) > div" \
  fixtures/creatine_products.html
brokkr litehtml test creatine_products
```

`prepare` and `extract` shell out to a Node.js script (requires Node + pnpm; auto-installs dependencies on first use). Node is already required for Puppeteer-based Chrome capture.

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

## Results database

Query stored benchmarks with `brokkr results`:

```
brokkr results                                      # last 20 results
brokkr results -n 50                                # last 50
brokkr results 0b74fb6f                             # look up by UUID prefix
brokkr results --commit a65a                        # filter by commit
brokkr results --command 'bench read'               # filter by command
brokkr results --variant pipelined                  # filter by variant
brokkr results --compare a65a 911c                  # compare two commits side-by-side
brokkr results --compare-last                       # compare two most recent commits
brokkr results --compare-last --command hotpath      # compare with hotpath function diff
```

The compare view shows timing, output size, peak RSS, rewrite ratio, and blob distribution columns as applicable. Hotpath comparisons include function-level timing diffs.

## Quick runtime timing

`brokkr run` supports ad-hoc machine-readable timing without the full benchmark harness:

```
brokkr run --time -- --help
# elapsed_ms=52 build_ms=51 run_ms=1

brokkr run --json -- --version
# {"build_ms":...,"run_ms":...,"elapsed_ms":...}

brokkr run --json --runs 5 --no-build -- --version
# {"build_ms":0,"run_ms":...,"elapsed_ms":...,"runs":5,"min_ms":...,"median_ms":...,"p95_ms":...}
```

- `--time`: stable `key=value` timing line.
- `--json`: structured timing JSON.
- `--runs N`: executes the command N times (single build) and reports min/median/p95.
- `--no-build`: skips build and runs the existing release binary.

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

# Cross-project source trees for preview pipeline
[plantasjen.preview]
pbfhogg = "/home/folk/Programs/pbfhogg"
elivagar = "/home/folk/Programs/elivagar"
nidhogg = "/home/folk/Programs/nidhogg"
```

- `project` — which project this is (`pbfhogg`, `elivagar`, `nidhogg`, or `litehtml-rs`)
- `[hostname.datasets.*]` — named datasets with PBF variants, OSC diffs, PMTiles entries, and bounding box
- `xxhash` — optional XXH128 hash for file integrity checks (`sha256` accepted as alias during migration). Run `brokkr env` to see computed hashes for updating config
- `[hostname]` — per-host path overrides, port, drive configuration, and default cargo features; defaults to `data/`, `data/scratch/`, and cargo target dir
- `features` — cargo features appended to every build (`run`, `bench`, `hotpath`, `profile`, `verify`, `serve`, `ingest`, `update`). Not applied to `check`. CLI `--features` are additive on top

## License

Apache-2.0
