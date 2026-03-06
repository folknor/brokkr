# brokkr

Shared development tooling for pbfhogg, elivagar, and nidhogg. Single Rust binary installed via `cargo install --path ~/Programs/brokkr`.

## Bash rules
- Never use sed, find, awk, or complex bash commands. Write a script instead.
- Never chain commands with &&. Write a script instead.
- Never pipe commands with |. Write a script instead.
- Never read or write from /tmp. All data lives in the project.

## How it works

Invoked as `brokkr` from any project root. Reads `./brokkr.toml` for project detection (`project = "pbfhogg|elivagar|nidhogg"`). Commands are gated by project — running a pbfhogg-only command from elivagar's root produces an error.

Install: `cargo install --path ~/Programs/brokkr`

## Architecture

Single crate, single binary. No workspace.

### Source layout

- `src/main.rs` — `main()`, command dispatch, all `cmd_*` handler functions
- `src/cli.rs` — CLI definition (clap derive): `Cli`, `Command`, `BenchCommand`, `VerifyCommand`
- `src/context.rs` — `HarnessContext`, `BenchContext`, bootstrap helpers, worktree lifecycle
- `src/resolve.rs` — Path resolution helpers (PBF, OSC, bbox, data dirs, results DB)
- `src/project.rs` — `Project` enum (Pbfhogg/Elivagar/Nidhogg), `detect()` (delegates to `config::load()`), `require()` gating
- `src/config.rs` — `DevConfig`, `Dataset`, `PbfEntry`, `OscEntry`, `HostConfig`, `ResolvedPaths`, TOML parsing (single parse returns `(Project, DevConfig)`), hostname via libc
- `src/build.rs` — `BuildConfig`, `cargo_build()` (JSON message parsing for executable path), `project_info()` via cargo metadata
- `src/harness.rs` — `BenchHarness` (lockfile + SQLite + env + git), `run_internal()`, `run_external()`, `run_distribution()`
- `src/request.rs` — Shared request structs (`BenchRequest`, `HotpathRequest`, `ProfileRequest`, `ResultsQuery`)
- `src/db/mod.rs` — `ResultsDb` wrapper, re-exports
- `src/db/types.rs` — `StoredRow`, `Distribution`, `KvPair`, `HotpathData`
- `src/db/schema.rs` — Table definitions, column constants
- `src/db/write.rs` — Insert/record result rows
- `src/db/query.rs` — Query by UUID prefix, commit, command, variant; comparison queries
- `src/db/format.rs` — Result formatting: `format_table`, `format_details`, `format_compare`
- `src/db/compare.rs` — Side-by-side commit comparison logic
- `src/db/hotpath.rs` — Hotpath report formatting for result detail view
- `src/db/migrate.rs` — Migration framework (v0→v3), `run_migrations()`
- `src/output.rs` — Prefixed console output (`[build]`, `[bench]`, `[verify]`, etc.), subprocess runners (`run_captured`, `run_passthrough_timed`)
- `src/error.rs` — `DevError` enum (Io, Config, Build, Preflight, Subprocess, Lock, Database, Verify)
- `src/env.rs` — `EnvInfo` collection (hostname, kernel, governor, memory, drives, tool versions)
- `src/git.rs` — `GitInfo` (commit hash, dirty flag, branch)
- `src/lockfile.rs` — `LockGuard` (via `OwnedFd`) for exclusive bench/verify/hotpath access
- `src/hotpath_fmt.rs` — Hotpath JSON report formatting
- `src/pmtiles.rs` — PMTiles v3 parser (header, varint, directory decoding, stats)
- `src/oom.rs` — OOM protection (`protect_child`, `check_memory`, `MemoryRisk`)
- `src/preflight.rs` — Pre-benchmark system checks (`Check` enum framework)
- `src/profiler.rs` — Sampling profiler integration (perf/samply)
- `src/tools.rs` — External tool discovery and auto-download (osmium, osmosis, tilemaker, shortbread config), cache-first network checks
- `src/worktree.rs` — Git worktree creation/cleanup for retroactive benchmarking

### Project-specific modules

- `src/pbfhogg/` — 25 modules: benchmarks (read, write, merge, commands, extract, allocator, blob-filter, planetiler, all), verify (10 commands + all), hotpath, profile, download
- `src/elivagar/` — 12 modules: benchmarks (self, node-store, pmtiles, planetiler, tilemaker, all), verify, compare-tiles, download-ocean, hotpath, profile
- `src/nidhogg/` — 13 modules: server lifecycle (serve/stop/status), ingest, update, query, geocode, benchmarks (api, ingest), verify (batch, geocode, readonly), hotpath, profile. `mod.rs` has shared curl helpers and query constants.

## brokkr.toml format

Each project has a `brokkr.toml` in its root:

```toml
project = "pbfhogg"

[plantasjen]
data = "data"
scratch = "data/scratch"
target = "target"
port = 3033
drives.source = "nvme"
drives.data = "ssd"

[plantasjen.datasets.denmark]
origin = "Geofabrik"
download_date = "2026-02-20"
bbox = "12.4,55.6,12.7,55.8"
data_dir = "denmark-data"          # nidhogg only

[plantasjen.datasets.denmark.pbf.indexed]
file = "denmark-with-indexdata.osm.pbf"
sha256 = "3f1977fd..."
seq = 4704

[plantasjen.datasets.denmark.pbf.raw]
file = "denmark-raw.osm.pbf"
seq = 4704

[plantasjen.datasets.denmark.osc.4705]
file = "denmark-4705.osc.gz"
sha256 = "fa581f7b..."

[plantasjen.datasets.denmark.pmtiles.elivagar]
file = "denmark-elivagar.pmtiles"
sha256 = "9a3b2c1d..."
```

Top-level keys that aren't `project` are treated as hostname sections (unknown non-table keys are rejected). Datasets are host-scoped (no global `[datasets]` section). Path resolution: host config → defaults (`data/`, `data/scratch/`, cargo target dir).

### Dataset structure

- `pbf.<variant>` — PBF file entries keyed by variant name (e.g. `raw`, `indexed`, `locations`). Each has `file`, optional `sha256`, optional `seq`.
- `osc.<seq>` — OSC diff file entries keyed by sequence number. Each has `file`, optional `sha256`.
- `pmtiles.<variant>` — PMTiles archive entries keyed by variant name (e.g. `elivagar`). Each has `file`, optional `sha256`. Used by nidhogg `serve` and `bench tiles`.
- Top-level dataset fields: `origin`, `download_date`, `bbox`, `data_dir` (nidhogg only).

### CLI flags for variant/seq selection

- `--variant <name>` — selects from `pbf.<name>` in config. Default: `indexed` (pbfhogg), `raw` (elivagar/nidhogg).
- `--osc-seq <seq>` — selects from `osc.<seq>` in config. Auto-selects if exactly one OSC is configured.
- `--tiles <variant>` — selects from `pmtiles.<variant>` in config. Auto-selects if exactly one PMTiles entry is configured.

## Shared commands (all projects)

- `check` — clippy + tests (extra args forwarded to cargo test)
- `env` — hostname, kernel, governor, memory, drives, tool versions, dataset status
- `run` — build release binary and run with passthrough args; supports `--time` (stable key=value timing), `--json` (structured timing), `--runs N` (min/median/p95 summary), `--no-build` (skip build)
- `results [UUID]` — look up specific result by UUID prefix (shows full detail + hotpath report)
- `results [--commit X] [--compare A B] [--compare-last] [--command CMD] [--variant V] [-n N] [--top N]` — query/compare benchmark results from SQLite. `--top 0` shows all hotpath functions. `--compare-last --command hotpath` diffs two most recent hotpath runs.
- `clean` — remove scratch/temp files
- `hotpath [target]` — function-level timing/allocation profiling via `hotpath` feature. Elivagar supports targets: `pmtiles`, `node-store` (micro-benchmark hotpath). No target = main pipeline.
- `profile` — sampling profiler (perf/samply)
- `pmtiles-stats` — PMTiles v3 file statistics (zoom distribution, tile sizes, compression)

## Benchmark harness

`BenchHarness` provides:
- Exclusive lockfile (prevents parallel bench/verify/hotpath runs)
- SQLite result storage with git commit, hostname, env snapshot
- `run_internal(config, closure)` — in-process timing (N runs, min/avg/max)
- `run_external(config, binary, args)` — subprocess timing
- `run_distribution(config, closure)` — distribution timing (min/p50/p95/max)

Results in `.brokkr/results.db` per project (gitignored).

## Conventions

- Same clippy lints as pbfhogg (see `[lints.clippy]` in Cargo.toml)
- All output prefixed: `[build]`, `[bench]`, `[verify]`, `[hotpath]`, `[run]`, `[error]`
- `DevError` variants for structured error handling (no `.unwrap()`)
- Project gating via `project::require()` — wrong-project commands fail with helpful message
- Build uses `--message-format=json` to extract executable path from cargo output. `find_executable` prefers the binary whose file stem matches the package/bin name exactly (avoids picking e.g. `nidhogg-update` instead of `nidhogg` when a package produces multiple binaries).

## Subagents
Subagents must NOT run any shell commands. They write code only. Integration, building, and testing is done in the main conversation.
