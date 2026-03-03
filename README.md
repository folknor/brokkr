# brokkr

Command orchestrator and development utility for [pbfhogg](https://github.com/folkol/pbfhogg), elivagar, and nidhogg. Single Rust binary that provides benchmarking, verification, profiling, and operational commands across all three projects.

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
```

## Commands

### Shared (all projects)

| Command | Description |
|---------|-------------|
| `check` | Run clippy + tests (extra args forwarded to `cargo test`) |
| `env` | Show hostname, kernel, governor, memory, drives, tool versions, dataset status |
| `run` | Build (or `--no-build`) and run with passthrough args; supports `--time`, `--json`, `--runs N` |
| `results` | Query the results database (`.brokkr/results.db`) |
| `clean` | Remove scratch/temp files |
| `hotpath` | Function-level timing/allocation profiling via `hotpath` feature |
| `profile` | Sampling profiler (perf/samply) |
| `pmtiles-stats` | PMTiles v3 file statistics (zoom distribution, tile sizes, compression) |
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

**Other**: `compare-tiles` (feature count comparison between PMTiles archives), `download-ocean` (ocean shapefiles).

### nidhogg

**Server**: `serve`, `stop`, `status`.

**Operations**: `ingest` (PBF to disk format), `update` (diff application), `query`, `geocode`.

**Benchmarks**: `api` (query performance), `nid-ingest` (ingest performance).

**Verification**: `batch` (batch query), `nid-geocode`, `readonly` (read-only filesystem).

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

[datasets.denmark]
pbf = "denmark-latest.osm.pbf"
osc = "denmark-diff.osc.gz"
pbf_raw = "denmark-latest-raw.osm.pbf"
bbox = "8.0,54.5,13.0,58.0"

[datasets.japan]
pbf = "japan-latest.osm.pbf"
bbox = "122.0,20.0,154.0,46.0"

# Host-specific overrides (matched by hostname)
[plantasjen]
data = "data"
scratch = "data/scratch"
target = "target"
port = 3033
drives.source = "/dev/nvme0n1p2"
drives.data = "/dev/sda1"
```

- `project` — which project this is (`pbfhogg`, `elivagar`, or `nidhogg`)
- `[datasets.*]` — named datasets with PBF path, OSC diff, raw PBF, and bounding box
- `[hostname]` — per-host path overrides, port, and drive configuration; defaults to `data/`, `data/scratch/`, and cargo target dir

## License

Apache-2.0
