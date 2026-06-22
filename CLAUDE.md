# brokkr

Shared development tooling for pbfhogg, elivagar, nidhogg, litehtml-rs, sluggrs, ratatoskr, and piners. Single Rust binary installed via `cargo install --path ~/Programs/brokkr`.

## Bash rules
- Never use sed, find, awk, or complex bash commands. Write a script instead.
- Never chain commands with &&. Write a script instead.
- Never chain commands with ;. Write a script instead.
- Never pipe commands with |. Write a script instead.
- Never capture stdout into env vars (`UUID=$(...)`) - shell state doesn't persist between tool calls. Read the output directly and use the value inline.
- Never read or write from /tmp. All data lives in the project.
- Prefer `brokkr check` over `cargo build` / `cargo clippy` / `cargo test`.

## Subagents
Subagents must NOT run any shell commands. They write code only. Integration, building, and testing is done in the main conversation.

## How it works

Invoked as `brokkr` from any project root. Reads `./brokkr.toml` for project detection (`project = "pbfhogg|elivagar|nidhogg|litehtml-rs|sluggrs|ratatoskr|piners"`). Commands are gated by project - running a pbfhogg-only command from elivagar's root produces an error.

Install: `cargo install --path ~/Programs/brokkr`

## Detailed docs

These files are not auto-loaded - read them on demand based on what the user asks. All `./docs/*` files must be 200 lines or less. Don't `wc` them before reading - just Read them.

- `docs/brokkr.toml.md` - **read when** the user asks about config fields, host sections, the `[gremlins]` exclude list, `[[check]]`, `[test]` profiles, `[litehtml]`, or `[ratatoskr]` blocks.
- `docs/brokkr.toml.datasets.md` - **read when** the user asks about `[<host>.datasets.*]` (pbf/osc/pmtiles entries) or the variant-selection CLI flags (`--variant`, `--osc-seq`, `--tiles`, `--snapshot`, `--as-snapshot`, `--direct-io`, `--io-uring`, `--compression`, `--locations-on-ways`) - map-data projects only (pbfhogg/elivagar/nidhogg).
- `docs/commands/deps.md` - **read when** the user asks about `brokkr deps` - the dependency-audit command (any Rust+git repo, not project-gated): the phase model, the `duplicate_version`/`git_dependency`/`path_dependency`/`outdated`/`stale` phases, focus mode (`brokkr deps <pkg>`), the `ccu --json` shell-out, exit codes, or the planned `advisory` phase.
- `docs/commands/check.md` - **read when** working on `brokkr check` or `brokkr test`, the gremlins/clippy/test pipeline, sweep selection, profile resolution, libtest filters, or the `BROKKR_TEST_BIN_DIR` contract.
- `docs/commands/visual.md` - **read when** the project is litehtml-rs or sluggrs and the user asks about `visual`, `list`, `approve`, `report`, `visual-status`, `prepare`, `html-extract`, or `outline`.
- `docs/commands/sync.md` - **read when** the project is ratatoskr and the user asks about `mock-serve`, `sync-list`, `sync-smoke`, or `sync-bench`. Covers sæhrimnir orchestration, readiness sentinel parsing, endpoint env-var export, marker FIFO usage.
- `docs/commands/ratatoskr-gate.md` - **read when** the project is ratatoskr and the user asks about `--gate`, `--as-baseline`, the `[ratatoskr.gate.*]` config block, baseline pinning per hostname, gate.db, or sync-bench regression thresholds (max/max_relative/max_delta/min/min_relative/equal/equal_to_baseline).
- `docs/commands/service.md` - **read when** the project is ratatoskr and the user asks about `service-test`, `service-suite`, or `service-list`. Covers lua VM, frontmatter, ceiling, artefact layout.
- `docs/commands/corpus.md` - **read when** the project is piners and the user asks about driving `brokkr corpus` - the parity-corpus runner, the `pins.toml`/keyword registry, probe selection (`--keyword`/`--probe`/`--all`/`--verify-only`), xxh128 verification, the expected-disposition gate, reseed/bless, or exit codes.
- `docs/commands/lint-corpus.md` - **read when** the project is piners and the user asks about `brokkr lint-corpus` / `brokkr lint-results` - the differential-lint corpus (piners vs pine-lint offline, gated on an agreement disposition), the `lints.toml` registry, the `(line,col,severity)` diff and dispositions, the `--reanchor` TV mode, `--bless`, or the `[piners.lint]` config block.
- `docs/brokkr.toml.piners.md` - **read when** the user asks about the `[piners]` config block (`corpus_root`, `registry_dir`, `feeds`, `harness`).
- `docs/commands/measure.md` - **read when** the user asks about `--bench`, `--hotpath`, `--alloc`, `--stop`, the sidecar profiler, the marker FIFO, `BenchHarness`, hotpath JSON contract, or `brokkr sidecar` queries.
- `docs/projects/piners.md` - **read when** the project is piners and the user asks about the harness NDJSON/manifest contracts, the `trade_diff` shape, the `runs.db` corpus run store and its schema, or the `brokkr corpus-results` query surface (piners-only).
- `docs/projects/pbfhogg.md` - **read when** working on pbfhogg-specific commands, verify subcommands, snapshot graph, OSC parser, io_uring/direct-io constraints, or the download command.
- `docs/projects/elivagar.md` - **read when** working on elivagar-specific commands.
- `docs/projects/nidhogg.md` - **read when** working on nidhogg-specific commands, server lifecycle, or the API client.
- `docs/projects/litehtml.md` - **read when** working on litehtml/sluggrs internals (modules, fixture preprocessing, Node.js scripts).
- `docs/projects/ratatoskr.md` - **read when** working on the ratatoskr harness model, sæhrimnir contract, fixture resolution, lua test runtime, or artefact layout.

## Architecture

Single crate, single binary. No workspace.

### Source layout

- `src/main.rs` - `main()`, command dispatch, `run_measured()`, `resolve_mode()`
- `src/cli/` - CLI definition (clap derive), split into `schema.rs` (`Cli`, `Command` incl. `Command::Deps` and all measurable commands, `ModeArgs`, `PbfArgs`, `VerifyCommand`, `Command::as_pbfhogg()`) and `validation.rs` (clap value parsers). All commands are top-level - no subcommand enums for litehtml/sluggrs
- `src/cargo_filter.rs` - Formatter primitives (`ClippyDiagnostic`, `ClippyParse`) plus the legacy text-output parser still used as a fallback by the test-phase build-error path. See the module header for why the JSON path replaced text scraping
- `src/cargo_json.rs` - JSON event model and parser for `check`. `CheckEvent` enum (Diagnostic, TestFailure, TestHung, DiagnosticSummary, TestSummary, Gremlin, GremlinSummary) serialized as NDJSON
- `src/gremlins.rs` - Gremlin detector for `brokkr check`. Scans `.rs`/`.toml`/`.md`/`.js`/`.sh` files (tracked + untracked-not-gitignored) for invisible/deceptive Unicode
- `src/scope.rs` - Scope + limit helpers. `changed_files()` computes files modified on the current branch via git merge-base; `partition()` sorts diagnostics scoped-first; `format_trailer()` builds the overflow summary
- `src/measure.rs` - `MeasureMode` (Run/Bench/Hotpath/Alloc), `MeasureRequest`, `CommandContext`
- `src/{pbfhogg,elivagar,nidhogg}/dispatch.rs` - Per-project dispatch (split from the old unified `src/dispatch.rs` in 0313f74). Pbfhogg exposes `run_command_with_params()`; elivagar and nidhogg expose `run_command()`. Pbfhogg and elivagar use `BenchContext` for build+harness; nidhogg delegates to per-module functions
- `src/pbfhogg/commands.rs` - `PbfhoggCommand` enum, single source of truth for argument construction
- `src/elivagar/commands.rs` - `ElivagarCommand` enum (Tilegen, PmtilesWriter, NodeStore, Planetiler, Tilemaker)
- `src/context.rs` - `HarnessContext`, `BenchContext`, bootstrap helpers, worktree lifecycle
- `src/resolve.rs` - Path resolution helpers (PBF, OSC, bbox, data dirs, results DB)
- `src/project.rs` - `Project` enum (Pbfhogg/Elivagar/Nidhogg/Litehtml/Sluggrs/Ratatoskr/Piners), `detect()`, `require()` gating
- `src/artefacts.rs` - `ArtefactDir`: per-run `<parent>/<test_id>/run-N/` allocator with preserve-on-failure semantics. Shared by ratatoskr (`.brokkr/ratatoskr`) and piners (`.brokkr/piners`)
- `src/config.rs` - `DevConfig`, `Dataset`, `PbfEntry`, `OscEntry`, `HostConfig`, `LitehtmlConfig`, `LitehtmlFixture`, `RatatoskrConfig`, `HarnessConfig`, `ResolvedPaths`, TOML parsing, hostname via libc
- `src/build.rs` - `BuildConfig`, `cargo_build()` (JSON message parsing for executable path), `project_info()` via cargo metadata
- `src/harness.rs` - `BenchHarness` (lockfile + SQLite + env + git), `run_internal()`, `run_external()`, `run_distribution()`
- `src/request.rs` - `ResultsQuery` / `SidecarQuery` structs
- `src/db/` - ResultsDb, SidecarDb, schema, migrations, queries, formatting, comparison
- `src/sidecar.rs` - Monitoring sidecar: `/proc` sampling, FIFO marker protocol. Always-on for measured modes
- `src/output.rs` - Prefixed console output (`[build]`, `[bench]`, `[verify]`, `[hotpath]`, `[run]`, `[sidecar]`, `[error]`), subprocess runners
- `src/error.rs` - `DevError` enum (Io, Config, Build, Preflight, Subprocess, Lock, Database, Verify)
- `src/lockfile.rs` - `LockGuard` (via `OwnedFd`)
- `src/oom.rs` - OOM protection (`protect_child` marks child as the kernel OOM killer's preferred target)
- `src/preflight.rs` - Pre-benchmark system checks
- `src/tools.rs` - External tool discovery and auto-download (osmium, osmosis, tilemaker, shortbread config)
- `src/worktree.rs` - Persistent git worktrees for retroactive benchmarking
- `src/history.rs` - `HistoryDb` - global command history at `$XDG_DATA_HOME/brokkr/history.db`
- `src/deps/` - `brokkr deps` dependency audit (any Rust+git repo, not project-gated). Phase-based like `check`: `mod.rs` (`DepsEvent` enum, `run()`, cargo-metadata deserializer, text + NDJSON renderers), `duplicate_version.rs` (blame-aware duplicate detection), `git_dependency.rs`, `path_dependency.rs`, `ccu.rs` (`ccu --json` shell-out feeding the outdated + stale phases), `focus.rs` (`brokkr deps <pkg>` chain trace). See `docs/commands/deps.md`.

### Project-specific modules

- `src/pbfhogg/` - benchmarks, verify (11 commands + all), download. See `docs/projects/pbfhogg.md`.
- `src/osc.rs` - Minimal `.osc` / `.osc.gz` reader for verify-side delta analysis. See module header and `docs/projects/pbfhogg.md`.
- `src/profile.rs` - Validation profile resolver for `[test.profiles.*]`. See module header and `docs/commands/check.md`.
- `src/elivagar/` - benchmarks, verify, compare-tiles, download-ocean, hotpath. See `docs/projects/elivagar.md`.
- `src/nidhogg/` - server lifecycle, ingest, update, query, geocode, benchmarks, verify. See `docs/projects/nidhogg.md`.
- `src/litehtml/` - 4 modules: visual reference testing. See `docs/projects/litehtml.md`.
- `src/ratatoskr/` - harness orchestration (`saehrimnir.rs`, `sync.rs`, `cmd.rs`, `discover.rs`). See `docs/projects/ratatoskr.md`.
- `src/piners/` - `brokkr corpus` parity-corpus runner: `registry.rs` (pins.toml + keyword loading, xxh128 verification), `pins_write.rs` (comment-preserving `pins.toml` writer via `toml_edit`, shared by `--reseed`/`--bless`), `select.rs` (selection resolution), `manifest.rs` (harness manifest), `report.rs` (NDJSON parse/render, incl. `trade_diff` collection), `cmd.rs` (orchestration + run persistence), `corpus_db/` (the `runs.db` SQLite store: schema/migrate/ingest/query/format, mirroring `src/db`), `corpus_query.rs` (the `brokkr corpus-results` handler). See `docs/commands/corpus.md` and `docs/projects/piners.md`.
- `src/piners/lint/` - `brokkr lint-corpus` / `brokkr lint-results`: the differential-lint corpus (piners vs pine-lint offline, gated on an agreement disposition). `mod.rs` (`Severity`/`DiagKey`/`DiagSet`/`ProbeResult` types, disposition labels, `now_rfc3339`), `registry.rs` (`lints.toml` + keyword loading, xxh128 verify, the TV anchor), `select.rs`, `validators.rs` (piners + pine-lint JSON parsers, normalized to `(line,col,severity)`), `diff.rs` (disposition + signature classifier), `lints_write.rs` (comment-preserving `lints.toml` writer, shared by `--bless`/`--reanchor`), `cmd.rs` (orchestration: build validator, per-probe run, gate, ingest, reanchor, bless), `db.rs` (the single-file `runs.db` store), `query.rs` (the `lint-results` handler). See `docs/commands/lint-corpus.md`.
- `scripts/litehtml-prepare/` - Node.js fixture preprocessing (cheerio + pngjs).

## Shared commands quick reference

For details, read the linked docs.

- `check` / `test` - validation pipeline. See `docs/commands/check.md`.
- `deps` - dependency audit of `Cargo.lock` / `cargo metadata` (any Rust+git repo, not project-gated). Phases: duplicate versions (with blame), git deps, out-of-workspace path deps, plus informational outdated/stale via `ccu --json`. `brokkr deps <pkg>` is focus mode (chain trace). Supports `--json`, `--limit`, `--all`, `--no-fail`. Exit 1 on offline findings. See `docs/commands/deps.md`.
- `env` - hostname, kernel, governor, memory, drives, tool versions, dataset status.
- `wc [threshold]` - list tracked `.rs` files with more than `threshold` lines (default 800), largest first. Works in any project.
- `results` - query the results database (`.brokkr/results.db`). Bare `brokkr results` shows a table of the last `-n` results (default 20). Supports `--commit`, `--compare`, `--command`, `--variant`, `-n`, `--top`. Uniform across all projects (piners included, for its hotpath/alloc runs).
- `corpus-results` - **[piners]** query the corpus run store (`.brokkr/piners/corpus/runs.db`) written by `brokkr corpus`, with the corpus flags (`--probe`/`--diffs`/`--trend`/`--run`/`--runtimes`/`--where`/`--sql`/`--full`). The query sibling of `corpus`; split out of `results` once piners gained benchmark runs. See `docs/projects/piners.md`.
- `lint-corpus` / `lint-results` - **[piners]** the differential-lint corpus: run `.pine` snippets through piners (dirty tree) and pine-lint offline, diff diagnostics on `(line,col,severity)`, gate on a pinned agreement disposition. `--reanchor` refreshes the TV anchor via `pine-lint --tv`; `--bless` stamps dispositions. `lint-results` queries `.brokkr/piners/lint/runs.db`. See `docs/commands/lint-corpus.md`.
- `clean [--worktrees]` - remove scratch/temp files. On ratatoskr projects also wipes `.brokkr/ratatoskr/` (run-N artefact dirs left by failed runs, plus `mock/` dirs from `mock-serve`). On piners projects removes the `.brokkr/piners/corpus/run-N/` dirs but **spares `runs.db`** (the corpus run store is the source of truth). `--worktrees` also purges all persistent benchmark worktrees.
- `pmtiles-stats` - PMTiles v3 file statistics (zoom distribution, tile sizes, compression).
- `history` - browse global command history log (`$XDG_DATA_HOME/brokkr/history.db`). Supports `--command`, `--project`, `--failed`, `--since`, `--slow`, `-n`, `--all`.
- `kill [--hard]` - cooperatively terminate the brokkr process holding the lock. Default sends SIGTERM (graceful: SIGKILLs child, flushes partial sidecar data under `dirty` alias, releases lock, runs `brokkr clean`). `--hard` sends SIGKILL to brokkr + child. Exits 130 on graceful path.
- `sidecar <uuid>` - query sidecar profiler data. See `docs/commands/measure.md`.
- `passthrough` - build and run with raw passthrough args (hidden, for ad-hoc use).
- Measurement modes (`--bench`, `--hotpath`, `--alloc`, `--stop`) - see `docs/commands/measure.md`.

Project-specific commands are documented under `docs/commands/` and `docs/projects/`.

## Conventions

- All output prefixed: `[build]`, `[bench]`, `[verify]`, `[hotpath]`, `[run]`, `[error]`
- `DevError` variants for structured error handling (no `.unwrap()`)
- Project gating via `project::require()` - wrong-project commands fail with helpful message
- Build uses `--message-format=json` to extract executable path from cargo output. `find_executable` prefers the binary whose file stem matches the package/bin name exactly. When no expected name is provided, requires exactly one executable - errors if multiple are found.
