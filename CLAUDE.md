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

- `src/main.rs` — CLI definition (clap derive), command dispatch, all handler functions
- `src/project.rs` — `Project` enum (Pbfhogg/Elivagar/Nidhogg), detection from `brokkr.toml`, `require()` gating
- `src/config.rs` — `DevConfig`, `Dataset`, `HostConfig`, `ResolvedPaths`, TOML parsing, hostname via libc
- `src/build.rs` — `BuildConfig`, `cargo_build()` (JSON message parsing for executable path), `project_info()` via cargo metadata
- `src/harness.rs` — `BenchHarness` (lockfile + SQLite + env + git), `run_internal()`, `run_external()`, `run_distribution()`
- `src/db.rs` — `ResultsDb` (SQLite), schema creation, insert/query/compare
- `src/output.rs` — Prefixed console output (`[build]`, `[bench]`, `[verify]`, etc.), subprocess runners (`run_captured`, `run_streamed`, `run_timed`)
- `src/error.rs` — `DevError` enum (Io, Config, Build, Preflight, Subprocess, Lock, Database)
- `src/env.rs` — `EnvInfo` collection (hostname, kernel, governor, memory, drives, tool versions)
- `src/git.rs` — `GitInfo` (commit hash, dirty flag, branch)
- `src/lockfile.rs` — `LockGuard` for exclusive bench/verify/hotpath access
- `src/preflight.rs` — Pre-benchmark system checks
- `src/tools.rs` — External tool discovery and auto-download (osmium, osmosis)

### Project-specific modules

- `src/pbfhogg/` — 25 modules: benchmarks (read, write, merge, commands, extract, allocator, blob-filter, planetiler, all), verify (10 commands + all), hotpath, profile, download
- `src/elivagar/` — 11 modules: benchmarks (self, node-store, pmtiles, planetiler, tilemaker, all), compare-tiles, download-ocean, hotpath, profile
- `src/nidhogg/` — 13 modules: server lifecycle (serve/stop/status), ingest, update, query, geocode, benchmarks (api, ingest), verify (batch, geocode, readonly), hotpath, profile

## brokkr.toml format

Each project has a `brokkr.toml` in its root:

```toml
project = "pbfhogg"

[datasets.denmark]
pbf = "denmark-latest.osm.pbf"
osc = "denmark-diff.osc.gz"
pbf_raw = "denmark-latest-raw.osm.pbf"
bbox = "8.0,54.5,13.0,58.0"

[plantasjen]
data = "data"
scratch = "data/scratch"
target = "target"
port = 3033
drives.source = "/dev/nvme0n1p2"
drives.data = "/dev/sda1"
```

Top-level keys that aren't `project` or `datasets` are treated as hostname sections. Path resolution: host config → defaults (`data/`, `data/scratch/`, cargo target dir).

## Shared commands (all projects)

- `check` — clippy + tests (extra args forwarded to cargo test)
- `env` — hostname, kernel, governor, memory, drives, tool versions, dataset status
- `run` — build release binary and run with passthrough args
- `results` — query `.brokkr/results.db` (SQLite)
- `clean` — remove scratch/temp files
- `hotpath` — function-level timing/allocation profiling via `hotpath` feature
- `profile` — sampling profiler (perf/samply)

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
- Build uses `--message-format=json` to extract executable path from cargo output

## Subagents
Subagents must NOT run any shell commands. They write code only. Integration, building, and testing is done in the main conversation.
