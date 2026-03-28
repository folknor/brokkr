# brokkr

Shared development tooling for pbfhogg, elivagar, nidhogg, litehtml-rs, and sluggrs. Single Rust binary installed via `cargo install --path ~/Programs/brokkr`.

## Bash rules
- Never use sed, find, awk, or complex bash commands. Write a script instead.
- Never chain commands with &&. Write a script instead.
- Never pipe commands with |. Write a script instead.
- Never read or write from /tmp. All data lives in the project.

## How it works

Invoked as `brokkr` from any project root. Reads `./brokkr.toml` for project detection (`project = "pbfhogg|elivagar|nidhogg|litehtml-rs"`). Commands are gated by project — running a pbfhogg-only command from elivagar's root produces an error.

Install: `cargo install --path ~/Programs/brokkr`

## Architecture

Single crate, single binary. No workspace.

### Source layout

- `src/main.rs` — `main()`, command dispatch, `run_measured()`, `resolve_mode()`
- `src/cli.rs` — CLI definition (clap derive): `Cli`, `Command` (top-level commands including all measurable commands), `ModeArgs`, `PbfArgs`, `VerifyCommand`, `LitehtmlCommand`, `Command::as_pbfhogg()`
- `src/measure.rs` — `MeasureMode` (Run/Bench/Hotpath/Alloc), `MeasureRequest`, `CommandContext`
- `src/dispatch.rs` — Unified dispatch for all three projects: `run_pbfhogg_command_with_params()`, `run_elivagar_command()`, `run_nidhogg_command()`. Each routes through run/bench/hotpath/alloc based on command enum + mode. Pbfhogg and elivagar use `BenchContext` for build+harness; nidhogg delegates to per-module functions due to divergent lifecycles
- `src/pbfhogg/commands.rs` — `PbfhoggCommand` enum with `build_args()`, `build_hotpath_args()`, `result_command()`, `result_variant()`, `metadata()` — single source of truth for all pbfhogg command argument construction
- `src/elivagar/commands.rs` — `ElivagarCommand` enum (Tilegen, PmtilesWriter, NodeStore, Planetiler, Tilemaker)
- `src/context.rs` — `HarnessContext`, `BenchContext`, bootstrap helpers, worktree lifecycle
- `src/resolve.rs` — Path resolution helpers (PBF, OSC, bbox, data dirs, results DB)
- `src/project.rs` — `Project` enum (Pbfhogg/Elivagar/Nidhogg/Litehtml/Sluggrs), `detect()` (delegates to `config::load()`), `require()` gating
- `src/config.rs` — `DevConfig`, `Dataset`, `PbfEntry`, `OscEntry`, `HostConfig`, `LitehtmlConfig`, `LitehtmlFixture`, `ResolvedPaths`, TOML parsing (single parse returns `(Project, DevConfig)`), hostname via libc
- `src/build.rs` — `BuildConfig`, `cargo_build()` (JSON message parsing for executable path), `project_info()` via cargo metadata
- `src/harness.rs` — `BenchHarness` (lockfile + SQLite + env + git), `run_internal()`, `run_external()`, `run_distribution()`
- `src/request.rs` — `ResultsQuery` struct for the results command
- `src/db/` — ResultsDb, SidecarDb, schema, migrations, queries, formatting, comparison
- `src/sidecar.rs` — Monitoring sidecar: `/proc` sampling, FIFO marker protocol, `run_sidecar()`, `SidecarFifo`, `SidecarRunResult`. Always-on for all measured modes
- `src/output.rs` — Prefixed console output (`[build]`, `[bench]`, `[verify]`, `[hotpath]`, `[run]`, `[sidecar]`, `[error]`), subprocess runners (`run_captured`, `spawn_captured`, `run_passthrough_timed`)
- `src/error.rs` — `DevError` enum (Io, Config, Build, Preflight, Subprocess, Lock, Database, Verify)
- `src/lockfile.rs` — `LockGuard` (via `OwnedFd`) for exclusive access
- `src/oom.rs` — OOM protection (`protect_child`, `check_memory`, `MemoryRisk`)
- `src/preflight.rs` — Pre-benchmark system checks (`Check` enum framework)
- `src/tools.rs` — External tool discovery and auto-download (osmium, osmosis, tilemaker, shortbread config)
- `src/worktree.rs` — Git worktree creation/cleanup for retroactive benchmarking
- `src/history.rs` — `HistoryDb` — global command history at `$XDG_DATA_HOME/brokkr/history.db`

### Project-specific modules

- `src/pbfhogg/` — `commands.rs` (command registry), benchmarks (read, write, merge, commands, extract, allocator, blob-filter, planetiler, all), verify (10 commands + all), download
- `src/elivagar/` — `commands.rs` (`ElivagarCommand` enum with `build_args()`, `build_config()`, `needs_pbf()`, `output_files()`, `metadata()`), benchmarks (self, node-store, pmtiles, planetiler, tilemaker, all), verify, compare-tiles, download-ocean, hotpath
- `src/nidhogg/` — `commands.rs` (`NidhoggCommand` enum: Api/Ingest/Tiles with `id()`, `supports_hotpath()`, `needs_build()`, `needs_server()`, `metadata()`), server lifecycle (serve/stop/status), ingest, update, query, geocode, benchmarks (api, ingest, tiles), verify (batch, geocode, readonly), hotpath. `client.rs` has query/bbox helpers that derive API queries from dataset bbox.
- `src/litehtml/` — 4 modules: visual reference testing (`cmd.rs` command dispatch, `db.rs` MechanicalDb, `compare.rs` pixel/element comparison, `mod.rs` UUID generation). `cmd.rs` also handles `prepare`/`extract`/`outline` by shelling out to Node.js script.
- `scripts/litehtml-prepare/` — Node.js fixture preprocessing (cheerio + pngjs). `prepare.js` handles `prepare`, `extract`, and `outline` subcommands. Dependencies managed via pnpm (`package.json`, `pnpm-lock.yaml`).

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
features = ["linux-direct-io", "linux-io-uring"]

[plantasjen.datasets.denmark]
origin = "Geofabrik"
download_date = "2026-02-20"
bbox = "12.4,55.6,12.7,55.8"
data_dir = "denmark-data"          # nidhogg only

[plantasjen.datasets.denmark.pbf.indexed]
file = "denmark-with-indexdata.osm.pbf"
xxhash = "3f1977fd..."
seq = 4704

[plantasjen.datasets.denmark.pbf.raw]
file = "denmark-raw.osm.pbf"
seq = 4704

[plantasjen.datasets.denmark.osc.4705]
file = "denmark-4705.osc.gz"
xxhash = "fa581f7b..."

[plantasjen.datasets.denmark.pmtiles.elivagar]
file = "denmark-elivagar.pmtiles"
xxhash = "9a3b2c1d..."
```

Top-level keys that aren't `project` are treated as hostname sections (unknown non-table keys are rejected). Datasets are host-scoped (no global `[datasets]` section). Path resolution: host config → defaults (`data/`, `data/scratch/`, cargo target dir). Host `features` are cargo features appended to every build command (all measurable commands, `verify`, `serve`, `ingest`, `update`) — NOT applied to `check`. CLI `--features` are additive on top of host features (deduped).

### Dataset structure

- `pbf.<variant>` — PBF file entries keyed by variant name (e.g. `raw`, `indexed`, `locations`). Each has `file`, optional `xxhash` (XXH128), optional `seq`. `sha256` is accepted as an alias during migration.
- `osc.<seq>` — OSC diff file entries keyed by sequence number. Each has `file`, optional `xxhash`. `sha256` accepted as alias.
- `pmtiles.<variant>` — PMTiles archive entries keyed by variant name (e.g. `elivagar`). Each has `file`, optional `xxhash`. `sha256` accepted as alias. Used by nidhogg `serve` and `bench tiles`.
- Top-level dataset fields: `origin`, `download_date`, `bbox`, `data_dir` (nidhogg only).

### CLI flags for variant/seq selection

- `--variant <name>` — selects from `pbf.<name>` in config. Default: `indexed` (pbfhogg), `raw` (elivagar/nidhogg).
- `--osc-seq <seq>` — selects from `osc.<seq>` in config. Auto-selects if exactly one OSC is configured.
- `--tiles <variant>` — selects from `pmtiles.<variant>` in config. Auto-selects if exactly one PMTiles entry is configured.

## CLI model

Every measurable command is a top-level brokkr subcommand. Measurement mode is a flag:

```
brokkr <command> [--bench [N] | --hotpath [N] | --alloc [N]] [command options]
```

- No flag — build, run once, print timing. Acquires lockfile, no DB storage.
- `--bench` — full benchmark: lockfile, 3 runs (or N), best-of-N stored in DB.
- `--hotpath` — function-level timing via hotpath feature. 1 run (or N).
- `--alloc` — per-function allocation tracking via hotpath-alloc feature. 1 run (or N).

All measured modes automatically run a sidecar that samples `/proc` metrics at 100ms and provides `BROKKR_MARKER_FIFO` for phase markers. Sidecar data is stored in `.brokkr/sidecar.db` (gitignored). Sidecar data is preserved even when the child is OOM-killed.

Dataset paths resolve from `brokkr.toml` automatically. All flags go after the command name.

### Shared commands (all projects)

- `check` — clippy + tests (extra args forwarded to cargo test). Supports `--features` and `--no-default-features`
- `env` — hostname, kernel, governor, memory, drives, tool versions, dataset status
- `results` — query the results database (`.brokkr/results.db`). Supports `--commit`, `--compare`, `--compare-last`, `--command`, `--variant`, `-n`, `--top`
- `clean` — remove scratch/temp files
- `pmtiles-stats` — PMTiles v3 file statistics (zoom distribution, tile sizes, compression)
- `history` — browse global command history log (`$XDG_DATA_HOME/brokkr/history.db`). Supports `--command`, `--project`, `--failed`, `--since`, `--slow`, `-n`, `--all`
- `passthrough` — build and run with raw passthrough args (hidden, for ad-hoc use)

## Litehtml commands (`brokkr litehtml <subcommand>`)

Gated to `project = "litehtml-rs"`. Visual reference testing — renders HTML fixtures through a pipeline binary, compares against Chrome screenshots.

- `test [fixture] [--suite S] [--all] [--recapture]` — run fixtures against Chrome reference artifacts. Builds pipeline binary, produces pixel diff + element match comparison.
- `list` — show configured fixtures with tags, expected outcome, and approval state
- `approve <fixture>` — record current divergence as accepted baseline (requires clean git tree)
- `report <run_id>` — show results table for a past test run
- `status` — dashboard: all fixtures with approved baseline vs last run, delta, improvements
- `prepare <input.html> <output.html>` — normalize raw email HTML into self-contained fixture (replaces images with correctly-sized gray PNGs, strips background-image/external CSS, injects Ahem font, pretty-prints). Shells out to Node.js script. Image cache in `.brokkr/prepare-cache/`.
- `extract <input.html> [--selector S | --from S --to S] <output.html>` — extract sub-fixture from prepared HTML. `--selector` for single element, `--from`/`--to` for sibling range. Preserves ancestor context and table cell stubs.
- `outline <input.html> [--depth N] [--full] [--selectors]` — structural overview of prepared HTML showing sections, image dimensions, text previews, and suggested CSS selectors for extract.

### Litehtml config in brokkr.toml

```toml
[litehtml]
viewport_width = 800
mode = "ahem"
pixel_diff_threshold = 0.5
element_match_threshold = 95.0
fallback_aspect_ratio = 2.0  # optional, for prepare command

[[litehtml.fixture]]
id = "creatine_hero"
path = "fixtures/creatine_hero/creatine_hero.html"
tags = ["creatine"]
expected = "pass"
```

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
- Build uses `--message-format=json` to extract executable path from cargo output. `find_executable` prefers the binary whose file stem matches the package/bin name exactly (avoids picking e.g. `nidhogg-update` instead of `nidhogg` when a package produces multiple binaries). When no expected name is provided, requires exactly one executable — errors if multiple are found.

## Sidecar profiler

The sidecar is always-on for all measured modes. It samples `/proc/{pid}/stat`, `/proc/{pid}/io`, and `/proc/{pid}/status` at 100ms intervals and reads phase markers from a FIFO. All data is buffered in memory during the run and bulk-inserted to `.brokkr/sidecar.db` (gitignored) after the child exits. Results DB (`.brokkr/results.db`) stays small and git-tracked.

Key files: `src/sidecar.rs` (core), `src/harness.rs` (`run_external`, `run_external_with_kv`, `run_hotpath_capture` — all sidecar-enabled), `src/db/sidecar.rs` (`SidecarDb`).

The child process receives `BROKKR_MARKER_FIFO` env var pointing to a named pipe. Stdout/stderr are drained in background threads to prevent pipe-buffer deadlock. Child exit is detected via `try_wait()` and the exact exit time is recorded for wall-clock measurement. Sidecar data is stored even when the child fails (OOM, signal, non-zero exit).

Query sidecar data with `brokkr results <uuid> --timeline` (JSONL), `--timeline --summary` (phase table), `--timeline --stat <field>`, `--markers --durations`, `--markers --phases`, `--compare-timeline <a> <b>`. The `dirty` pseudo-UUID resolves to the most recent failed/dirty-tree run.

## Removed features

- `--profile` flag and `Command::Profile` removed in b17a219. Previously did two-pass hotpath (pbfhogg) or sampling profiler via perf/samply (elivagar). Restore from that commit if elivagar needs sampling profiler support again.
- `Command::Bench`, `Command::Hotpath`, `BenchCommand` enum removed in 893e3fd. Replaced by top-level measured commands with `--bench`/`--hotpath`/`--alloc` flags.
- `src/profiler.rs` (perf/samply integration) removed in 6c8d846. Restore from that commit if needed.

## Subagents
Subagents must NOT run any shell commands. They write code only. Integration, building, and testing is done in the main conversation.
