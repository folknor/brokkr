# brokkr

Shared development tooling for pbfhogg, elivagar, nidhogg, litehtml-rs, sluggrs, ratatoskr, piners, and dellingr. Single Rust binary installed via `cargo install --path ~/Programs/brokkr`.

## Bash rules
- Never use sed, find, awk, or complex bash commands. Write a script instead.
- Never chain commands with &&. Write a script instead.
- Never chain commands with ;. Write a script instead.
- Never pipe commands with |. Write a script instead.
- Never capture stdout into env vars (`UUID=$(...)`) - shell state doesn't persist between tool calls. Read the output directly and use the value inline.
- Never read or write from /tmp. All data lives in the project.
- Never use raw `cargo`. Use `brokkr check` or `brokkr test`.

## Subagents
Subagents must NOT run any shell commands. They write code only. Integration, building, and testing is done in the main conversation.

## Code review (`review`)

`review` fans a code review out to fresh AI sessions. Config is `.review.toml` in the repo root: `[archetypes]` (name = priming prompt), `[_defaults].providers`, and `--profile` overrides scoped `[<host>.<provider>.<profile>]`. This repo currently defines the `bugs` archetype against the `codex` provider, with `bugs` and `implement` profiles for the `plantasjen` host.

```
echo "Please review the unstaged changes and report your findings." | review bugs --profile bugs
```

- **Piping into `review` is the one sanctioned exception to the no-`|` bash rule above.**
- **Keep the piped prompt to one line.** The archetype prime already tells the session to inspect the current repository state, and each run starts fresh and fetches the code itself. A long prompt that summarises the diff and pre-states where the bugs probably are defeats the point - the session's value is that it reaches its own conclusions, and a reviewer told what to think tends to agree. Say what to look at, not what to find.
- Each run is a fresh session; the printed session ID resumes it via `--session` while the cache is warm.
- `--dry-run` prints the assembled prompt instead of sending it.

## How it works

Invoked as `brokkr` from any project root. Reads `brokkr.toml` for project detection (`project = "pbfhogg|elivagar|nidhogg|litehtml-rs|sluggrs|ratatoskr|piners|dellingr"`). Commands are gated by project - running a pbfhogg-only command from elivagar's root produces an error.

Dellingr's surface is one command, `brokkr dellingr --lua <workload>`, plus the shared commands. Its bench shape removes most of what the map-data projects resolve (no datasets, no scratch, no drives): a workload is a hash-pinned `.lua` file in the repo, and the harness is a cargo *example* target whose features the measurement mode picks. See `docs/projects/dellingr.md`.

`brokkr.toml` is looked up in the working directory, or one level up (its immediate parent) if absent there. The one-level-up form is for driving a checkout that isn't ours: the config and everything brokkr owns (`data/`, `.brokkr/` with `results.db`) live in the parent, keeping the foreign repo clean, while git and cargo still run against the working directory. Detection splits these as `project_root` (config dir - data/`.brokkr`) vs `build_root` (cwd - git/cargo); they coincide in the common case (config in cwd). See `src/project.rs`.

Install: `cargo install --path ~/Programs/brokkr`

## Detailed docs

These files are not auto-loaded - read them on demand based on what the user asks. Don't `wc` them before reading - just Read them.

They are also compiled into the binary and readable as `brokkr man <topic>` (bare `brokkr man` lists the topics available in the current project). The topic table lives in `src/man.rs` and is the executable version of the list below: it uses `include_str!`, so a renamed or deleted doc breaks the build instead of leaving a stale pointer here. Topics are project-filtered by the same `Visibility` the CLI uses, except the project-agnostic ones (`config`, `check`, `clippy`, `deps`, `measure`, `output-channels`), which list everywhere. When editing this list, update `TOPICS` to match.

- `docs/brokkr.toml.md` - **read when** the user asks about config fields, host sections, the `[gremlins]` exclude list, `[[check]]`, `[test]` profiles, `[litehtml]`, `[ratatoskr]`, or `[<host>.tilegen.*]` blocks (the elivagar tilegen contract: ocean inputs, budgets, geometry - `brokkr tilegen` has no override flags).
- `docs/brokkr.toml.datasets.md` - **read when** the user asks about `[<host>.datasets.*]` (pbf/osc/pmtiles entries) or the variant-selection CLI flags (`--variant`, `--osc-seq`, `--tiles`, `--snapshot`, `--as-snapshot`, `--direct-io`, `--io-uring`, `--compression`, `--locations-on-ways`) - map-data projects only (pbfhogg/elivagar/nidhogg).
- `docs/commands/deps.md` - **read when** the user asks about `brokkr deps` - the dependency-audit command (any Rust+git repo, not project-gated): the phase model, the `duplicate_version`/`git_dependency`/`path_dependency`/`outdated`/`stale` phases, focus mode (`brokkr deps <pkg>`), the `ccu --json` shell-out, exit codes, or the planned `advisory` phase.
- `docs/commands/check.md` - **read when** working on `brokkr check` or `brokkr test`, the gremlins/clippy/test pipeline, sweep selection, profile resolution, libtest filters, or the `BROKKR_TEST_BIN_DIR` contract.
- `docs/commands/clippy.md` - **read when** working on `brokkr clippy` - the investigative single-phase clippy runner (ad-hoc `-p` + real `--all-features`, or `--sweep NAME` to replay one `[[check]]` entry), the ad-hoc env-union rule and `--env` overrides, or how it reuses `check`'s clippy pipeline (`src/check_cmd/phase.rs::cmd_clippy`).
- `docs/commands/visual.md` - **read when** the project is litehtml-rs or sluggrs and the user asks about `visual`, `list`, `approve`, `report`, `visual-status`, `prepare`, `html-extract`, or `outline`.
- `docs/commands/hotpath.md` - **read when** the project is sluggrs and the user asks about `brokkr hotpath` - the sluggrs rendering bench: the mode->feature mapping (`--hotpath` / `--alloc` instrumented, `--bench` bare), the `--target` double naming (`hotpath` -> example `hotpath`, filed as `render`; anything else -> `NAME_bench`), or `--commit` (no split-tree rule - unlike dellingr, everything measured is code).
- `docs/commands/sync.md` - **read when** the project is ratatoskr and the user asks about `sync` or `mock-serve`. `brokkr sync` is one command: bare lists, `<SCRIPT>` runs, `--all` runs the discovered cohort, `<SCRIPT> --bench [N]` measures, `--gate all --bench [N]` sweeps every configured gate. Replaced the former `sync-list`/`sync-smoke`/`sync-bench` triple. Covers sæhrimnir orchestration, readiness sentinel parsing, endpoint env-var export, marker FIFO usage, and the bench path's `--commit` split-tree rule (harness from the worktree; script, fixture and sæhrimnir from the current tree - dellingr's rule, not sluggrs').
- `docs/commands/ratatoskr-gate.md` - **read when** the project is ratatoskr and the user asks about `--gate`, the `--gate all` cohort sweep (and why `--as-baseline` is refused alongside it), `--as-baseline`, the `[ratatoskr.gate.*]` config block, baseline pinning per hostname, gate.db (and its exemption from `brokkr clean`), bisecting a breach with `--commit`, or sync bench regression thresholds (max/max_relative/max_delta/min/min_relative/equal/equal_to_baseline).
- `docs/commands/service.md` - **read when** the project is ratatoskr and the user asks about `service`. One command in the bare-is-an-index shape: bare lists, `<SCRIPT>` runs one (a directory runs that cohort), `--all` runs every discovered script against one shared build. Replaced the former `service-test`/`service-suite`/`service-list` triple. Covers lua VM, frontmatter, ceiling, artefact layout, fixture lifecycle.
- `docs/commands/corpus.md` - **read when** the project is piners and the user asks about driving `brokkr corpus` - the parity-corpus runner, the `pins.toml`/keyword registry, probe selection (`--keyword`/`--probe`/`--all`/`--verify-only`), xxh128 verification, the expected-disposition gate, reseed/bless, or exit codes.
- `docs/commands/lint-corpus.md` - **read when** the project is piners and the user asks about `brokkr lint-corpus` / `brokkr lint-results` - the differential-lint corpus (piners vs pine-lint offline, gated on an agreement disposition), the `lints.toml` registry, the `(line,col,severity)` diff and dispositions, the `--reanchor` TV mode, `--bless`, or the `[piners.lint]` config block.
- `docs/brokkr.toml.piners.md` - **read when** the user asks about the `[piners]` config block (`corpus_root`, `registry_dir`, `feeds`, `harness`).
- `docs/commands/measure.md` - **read when** the user asks about `--bench`, `--hotpath`, `--alloc`, `--stop`, the sidecar profiler, the marker FIFO, `BenchHarness`, hotpath JSON contract, or `brokkr sidecar` queries. Also covers the marker/counter name-interpretation rules per query view and how `brokkr sidecar` renders its JSONL/`--human` tables.
- `docs/commands/output-channels.md` - **read when** the user asks where a run's output goes - stdout vs stderr `key=value` vs FIFO markers/counters, which lands in `results.db` (`brokkr results`) vs `sidecar.db` (`brokkr sidecar`), the per-harness-path capture matrix (`run_external_ok`/`run_external_with_kv_raw`/`run_internal`/`run_passthrough_timed`), or the per-command table for pbfhogg + elivagar.
- `docs/projects/pbfhogg-vs-elivagar.md` - **read when** the user asks how pbfhogg and elivagar's dispatch layers differ - build kinds, bench harness path, timing source (external wall-clock vs self-reported stderr `elapsed_ms`), I/O-mode flags, output-artifact lifecycle, or which output channels each command feeds.
- `docs/projects/piners.md` - **read when** the project is piners and the user asks about the harness NDJSON/manifest contracts, the `trade_diff` shape, the `runs.db` corpus run store and its schema, or the `brokkr corpus-results` query surface (piners-only).
- `docs/projects/pbfhogg.md` - **read when** working on pbfhogg-specific commands, verify subcommands, snapshot graph, OSC parser, io_uring/direct-io constraints, or the download command.
- `docs/projects/elivagar.md` - **read when** working on elivagar-specific commands, incl. `regress` (two-explicit-archive output-regression diff; no baseline registry), `pmtiles-corpus` (wraps elivagar's git-committed output corpus - the standing gate that replaced the retired blessed-archive machinery), `ocean-build`, and the durable tilegen output store.
- `docs/projects/nidhogg.md` - **read when** working on nidhogg-specific commands, server lifecycle, or the API client.
- `docs/projects/dellingr.md` - **read when** the project is dellingr and the user asks about `brokkr dellingr` - the `[dellingr]` config block (`example` + `[dellingr.workloads.*]`), the xxh128 workload pin and what drift refusal means, the mode->feature mapping (bare / `hotpath` / `hotpath-alloc`), the deliberate two-tree `--commit` rule, the harness argv + sidecar marker contract, or why the `dataset` column is empty.
- `docs/projects/litehtml.md` - **read when** working on litehtml/sluggrs internals (modules, fixture preprocessing, Node.js scripts).
- `docs/projects/ratatoskr.md` - **read when** working on the ratatoskr harness model, sæhrimnir contract, fixture resolution, lua test runtime, or artefact layout.

## Architecture

Single crate, single binary. No workspace.

### Source layout

- `src/main.rs` - `main()`, command dispatch, `run_measured()`, `resolve_mode()`
- `src/cli/` - CLI definition (clap derive), split into `schema.rs` (`Cli`, `Command` incl. `Command::Deps` and all measurable commands, `ModeArgs`, `PbfArgs`, `VerifyCommand`, `Command::as_pbfhogg()`) and `validation.rs` (clap value parsers). All commands are top-level - no subcommand enums for litehtml/sluggrs. `visibility.rs` holds `TABLE`, the subcommand-name -> applicable-projects map that hides irrelevant commands from `--help` (see below); the three files are `include!`d into `src/cli.rs`, so they carry `//` comments, not `//!`

### Project-scoped help

`brokkr --help` lists only the commands that apply to the detected project - 18 in a brokkr checkout, not the full 87. `parse_cli()` in `src/main_parts/bootstrap.rs` detects the project *before* parsing, then walks `Cli::command()` marking every subcommand `cli::visible_in()` rejects as `hide(true)`, and parses from the mutated command via `FromArgMatches`.

Hidden is **not** disabled. A hidden subcommand still parses and still reaches its handler, where `project::require()` produces the usual `'brokkr X' is only available in Y projects` error. `visibility.rs` is therefore a presentation layer that must agree with those `require()` calls - it never becomes the gate itself.

Everything fails open: an undetectable or malformed `brokkr.toml` leaves the full list visible, and a subcommand missing from `TABLE` stays visible everywhere. Two tests in `src/cli.rs` keep the table honest - one fails if any subcommand is absent from `TABLE`, the other if `TABLE` names a subcommand that no longer exists
- `src/cargo_filter.rs` - Formatter primitives (`ClippyDiagnostic`, `ClippyParse`) plus the legacy text-output parser still used as a fallback by the test-phase build-error path. See the module header for why the JSON path replaced text scraping
- `src/cargo_json.rs` - Parser for cargo's `--message-format=json` diagnostics (`DiagnosticEvent`, `parse_cargo_diagnostics`), feeding check's text renderer. The old NDJSON `CheckEvent` output mode was removed (3ff8291); `check --json` now emits a single schema-versioned summary trailer built in `src/check_cmd/phase.rs` (`CheckSummary`)
- `src/gremlins.rs` - Gremlin detector for `brokkr check`. Scans `.rs`/`.toml`/`.md`/`.js`/`.sh` files (tracked + untracked-not-gitignored) for invisible/deceptive Unicode. Exposes `tracked_files()` (shared file walk) and `CodepointSet` (allow/ban singletons + ranges)
- `src/style.rs` - `[style]` native check: blank line above `if`/`match`/`for`/`while`/`loop`/`spawn`. Ported from nautilus's `check_formatting_rs`. Opt-in
- `src/header.rs` - `[header]` check: required file header with a current-year (`{year}`) requirement via libc `gmtime`. Ported from `check_copyright_year`
- `src/textlint.rs` - `[[textlint]]` engine: declarative forbid-a-regex-on-a-line rules with bounded predicates (`allow_marker`, `except`, `in_toml_section`, `table_row_only`). The generic engine for grep-style convention hooks
- `src/globs.rs` - `globset` wrapper for the path-glob lists shared by `header`/`textlint`
- `src/scope.rs` - Scope + limit helpers. `changed_files()` computes files modified on the current branch via git merge-base; `partition()` sorts diagnostics scoped-first; `format_trailer()` builds the overflow summary
- `src/measure.rs` - `MeasureMode` (Run/Bench/Hotpath/Alloc), `MeasureRequest`, `CommandContext`
- `src/{pbfhogg,elivagar,nidhogg}/dispatch.rs` - Per-project dispatch (split from the old unified `src/dispatch.rs` in 0313f74). Pbfhogg exposes `run_command_with_params()`; elivagar and nidhogg expose `run_command()`. Pbfhogg and elivagar use `BenchContext` for build+harness; nidhogg delegates to per-module functions
- `src/pbfhogg/commands.rs` - `PbfhoggCommand` enum, single source of truth for argument construction
- `src/elivagar/commands.rs` - `ElivagarCommand` enum (Tilegen, PmtilesWriter, NodeStore, Planetiler, Tilemaker)
- `src/context.rs` - `HarnessContext`, `BenchContext`, bootstrap helpers, worktree lifecycle
- `src/resolve.rs` - Path resolution helpers (PBF, OSC, bbox, data dirs, results DB)
- `src/project.rs` - `Project` enum (Pbfhogg/Elivagar/Nidhogg/Litehtml/Sluggrs/Ratatoskr/Piners), `detect()`, `require()` gating
- `src/man.rs` + `src/man/render.rs` - `brokkr man`: the `TOPICS` table (name, summary, `include_str!`'d markdown, `Visibility`) and a `pulldown_cmark` -> ANSI terminal renderer (headings, GFM tables, alerts, footnotes, nested lists; links keep their text and drop the URL). Renderer ported from mogwai's `mogwai-server/src/man/render.rs`
- `src/artefacts.rs` - `ArtefactDir`: per-run `<parent>/<test_id>/run-N/` allocator with preserve-on-failure semantics. Shared by ratatoskr (`.brokkr/ratatoskr`) and piners (`.brokkr/piners`)
- `src/config.rs` - `DevConfig`, `Dataset`, `PbfEntry`, `OscEntry`, `HostConfig`, `LitehtmlConfig`, `LitehtmlFixture`, `RatatoskrConfig`, `HarnessConfig`, `ResolvedPaths`, TOML parsing, hostname via libc
- `src/build.rs` - `BuildConfig`, `cargo_build()` (JSON message parsing for executable path), `project_info()` via cargo metadata
- `src/harness.rs` - `BenchHarness` (lockfile + SQLite + env + git), `run_internal()`, `run_external()`, `run_distribution()`
- `src/request.rs` - `ResultsQuery` / `SidecarQuery` structs
- `src/db/` - ResultsDb, SidecarDb, schema, migrations, queries, formatting, comparison
- `src/sidecar.rs` - Monitoring sidecar: `/proc` sampling, FIFO marker protocol. Always-on for measured modes
- `src/output.rs` - Prefixed console output (`[build]`, `[bench]`, `[verify]`, `[hotpath]`, `[run]`, `[sidecar]`, `[error]`), subprocess runners
- `src/error.rs` - `DevError` enum (Io, Config, Build, Preflight, Subprocess, Lock, Database, Verify)
- `src/lockfile.rs` - `LockGuard` (via `OwnedFd`); `acquire` is re-entrant within the process (nested acquires share one flock hold, released when the last guard drops) - what lets ratatoskr's `--gate all` cohort hold the lock across a whole sweep without self-deadlocking on a second flock fd (`sync --all` holds sweep-wide too but no longer nests - it builds once and runs scripts against the prebuilt harness)
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
- `src/elivagar/` - benchmarks, verify, download-ocean, hotpath, `compare_tiles.rs` (the native lenient per-layer sampling census; tolerant decode, no verdict), `regress/` (the native two-explicit-archive semantic diff; no comparability gate - regress is the tier-3 attribution instrument, comparability is the caller's job), `corpus/` (the native `pmtiles-corpus` gate - brokkr owns the adjudication; see below) and `ocean_build.rs` (`ocean-build`, derives the invocation from `[<host>.tilegen.default].ocean`). See `docs/projects/elivagar.md`.
- `src/elivagar/corpus/` + `src/elivagar/eliv.rs` - the corpus gate moved out of elivagar per the `brokkr.md`/`elivagar.md` redesign: brokkr owns the digest fold, gating policy, verdicts, SVG render core, mutate and calibration, and reads archives in-process through the linked elivagar crate. `eliv.rs` is the seam - the linked-crate surface (reader/decoder/writer) the gate and regress consume. `check` exits 0/1/2/3 (pass / content mismatch / archive refusal / baseline trouble). Modules: `canonical` (the FROZEN streaming tile hash + the detail canonical form + their equivalence tests), `digest` (fold + formats + self-integrity), `diff` (localized deltas), `contract` (comparability policy), `manifest`, `render` (canonical SVG core), `style`, `mutate` (calibration instrument), `fixture` (test-only shared MVT/PMTiles fixture encoder, also used by regress's tests).
- `src/elivagar/regress/` - the native two-archive semantic diff, ported from elivagar's shed `regress.rs`. `engine` (the three-pass raw/canonical/detail blob-pair engine), `prepared` (re-augments the wire-order `tile_detail` decode with bboxes/digests/structure signatures), `compare` (the `DiffSink` protocol + classification), `pairing` (exact, min-cost-matching residual, force-zip), `geometry` (exact-integer bbox/Hausdorff/KD-tree/hole-containment), `report` (bounded report + rendering), `overlay` (`--overlay` attribution SVGs). Pass 2 calls `corpus::canonical`'s hash, so regress and the gate share one definition of "the same tile".
- `src/nidhogg/` - server lifecycle, ingest, update, query, geocode, benchmarks, verify. See `docs/projects/nidhogg.md`.
- `src/litehtml/` - 4 modules: visual reference testing. See `docs/projects/litehtml.md`.
- `src/ratatoskr/` - harness orchestration (`saehrimnir.rs`, `sync.rs`, `cmd.rs`, `discover.rs`). See `docs/projects/ratatoskr.md`.
- `src/dellingr/` - `brokkr dellingr`, the Lua-workload bench. `workload.rs` (the `[dellingr.workloads.*]` registry: `--lua NAME` -> absolute path, xxh128 verified via `preflight::verify_file_hash`; resolution is anchored to `project_root` so a `--commit` run takes the harness from the worktree and the workload from the current tree), `cmd.rs` (build the `example` target with mode-dependent features, then `run_external` for `--bench` or `run_hotpath_capture` for `--hotpath`/`--alloc`). Rows are filed under the workload name with an empty `input_file`. See `docs/projects/dellingr.md`.
- `src/piners/` - `brokkr corpus` parity-corpus runner: `registry.rs` (pins.toml + keyword loading, xxh128 verification), `pins_write.rs` (comment-preserving `pins.toml` writer via `toml_edit`, shared by `--reseed`/`--bless`), `select.rs` (selection resolution), `manifest.rs` (harness manifest), `report.rs` (NDJSON parse/render, incl. `trade_diff` collection), `cmd.rs` (orchestration + run persistence), `corpus_db/` (the `runs.db` SQLite store: schema/migrate/ingest/query/format, mirroring `src/db`), `corpus_query.rs` (the `brokkr corpus-results` handler). See `docs/commands/corpus.md` and `docs/projects/piners.md`.
- `src/piners/lint/` - `brokkr lint-corpus` / `brokkr lint-results`: the differential-lint corpus (piners vs pine-lint offline, gated on an agreement disposition). `mod.rs` (`Severity`/`DiagKey`/`DiagSet`/`ProbeResult` types, disposition labels, `now_rfc3339`), `registry.rs` (`lints.toml` + keyword loading, xxh128 verify, the TV anchor), `select.rs`, `validators.rs` (piners + pine-lint JSON parsers, normalized to `(line,col,severity)`), `diff.rs` (disposition + signature classifier), `lints_write.rs` (comment-preserving `lints.toml` writer, shared by `--bless`/`--reanchor`), `cmd.rs` (orchestration: build validator, per-probe run, gate, ingest, reanchor, bless), `db.rs` (the single-file `runs.db` store), `query.rs` (the `lint-results` handler). See `docs/commands/lint-corpus.md`.
- `scripts/litehtml-prepare/` - Node.js fixture preprocessing (cheerio + pngjs).

## Shared commands quick reference

For details, read the linked docs.

- `check` / `test` - validation pipeline. See `docs/commands/check.md`.
- `clippy` - investigative single-phase clippy runner (not project-gated). Runs ONLY clippy against an ad-hoc target: `-p` (repeatable) + `--all-features`/`--features`/`--no-default-features`, or `--sweep NAME` to replay one `[[check]]` entry. Ad-hoc env is the union of every `[[check]]` entry's env; `--env KEY=VALUE` overrides. Same output as check's clippy phase (`--raw`/`--limit`/`--all`, any-diagnostic-fails). `disable_toolchain` + global lock apply. See `docs/commands/clippy.md`.
- `deps` - dependency audit of `Cargo.lock` / `cargo metadata` (any Rust+git repo, not project-gated). Phases: duplicate versions (with blame), git deps, out-of-workspace path deps, plus informational outdated/stale via `ccu --json`. `brokkr deps <pkg>` is focus mode (chain trace). Supports `--json`, `--limit`, `--all`, `--no-fail`. Exit 1 on offline findings. See `docs/commands/deps.md`.
- `man [TOPIC]` - read the bundled `docs/**.md` (compiled in via `include_str!`, rendered markdown->ANSI). Bare `brokkr man` lists the topics for the detected project; colour is dropped when stdout is not a TTY or `NO_COLOR` is set, so `brokkr man check | less` is plain text. A topic belonging to another project reports that rather than pretending not to exist. Works with no `brokkr.toml` at all - you still get the project-agnostic topics.
- `env` - hostname, kernel, governor, memory, drives, tool versions, dataset status.
- `wc [threshold]` - list tracked `.rs` files with more than `threshold` lines (default 800), largest first. Works in any project.
- `results` - query the results database (`.brokkr/results.db`). Bare `brokkr results` shows a table of the last `-n` results (default 20). Supports `--commit`, `--compare`, `--command`, `--variant`, `-n`, `--top`, `--grep`/`--grep-v` (substring match on the run's invocation - the two argv columns plus each captured env var as `NAME=VALUE`, so `--grep LAYER_STATS` finds an env-gated arm that appears in no argv; `--grep` ANDs, `--grep-v` excludes on any hit - the way to select an A/B arm defined by an *absent* flag), `--env NAME=VALUE` (exact match on a captured env var), `--meta`. `brokkr results <uuid>` shows per-iteration walls for `--bench N` rows and the `prev.*` provenance of what ran before it; `--compare A B` annotates pairs whose host conditions (memory/governor/kernel) differed. Two rows pair into one delta row when `(command, mode, input_file, brokkr_args, env_fingerprint)` match - `brokkr_args` is in the key so that arm-defining flags (`--direct-io` and kin) never collapse into one averaged row, but `--commit REF` and `--verbose`/`-v` are stripped first (`normalize_brokkr_args` in `src/db/format/compare.rs`): `--commit` *is* the comparison axis, so keying on it would stop a retro row from ever pairing with the current-tree row it exists to be compared against. Uniform across all projects (piners included, for its hotpath/alloc runs) except litehtml, whose only use of `results.db` is the unrelated `MechanicalDb` schema - so `results`, `sidecar` and `invalidate` are hidden from its `--help`.
- `corpus-results` - **[piners]** query the corpus run store (`.brokkr/piners/corpus/runs.db`) written by `brokkr corpus`, with the corpus flags (`--probe`/`--diffs`/`--trend`/`--run`/`--runtimes`/`--where`/`--sql`/`--full`). The query sibling of `corpus`; split out of `results` once piners gained benchmark runs. See `docs/projects/piners.md`.
- `lint-corpus` / `lint-results` - **[piners]** the differential-lint corpus: run `.pine` snippets through piners (dirty tree) and pine-lint offline, diff diagnostics on `(line,col,severity)`, gate on a pinned agreement disposition. `--reanchor` refreshes the TV anchor via `pine-lint --tv`; `--bless` stamps dispositions. `lint-results` queries `.brokkr/piners/lint/runs.db`. See `docs/commands/lint-corpus.md`.
- `clean [--worktrees] [--cargo [PKG]] [--archives [--keep N]] [--all] [--dry-run]` - remove scratch/temp files. Guiding principle: clean removes only what brokkr created, identified by a brokkr-designated directory or a *constructed* canonical name; the measurement DBs (results.db/sidecar.db/history.db, and piners' `runs.db`) are permanently out of scope. A routine clean also removes the per-sweep isolated target dirs (`<target>/rustflags-*`, the reproducible caches a `rustflags`-carrying `[[check]]` entry builds into). `--cargo` additionally runs `cargo clean -p <PKG>` (PKG defaults to the brokkr.toml project name) - wipes the project's own build artifacts across all profiles while keeping dependency caches, the fix for stale-incremental linker failures (phantom undefined `anon.*.llvm.*` symbols). On elivagar projects a routine clean wipes `tilegen_tmp` and `ocean-build_tmp` (scratch) and the `corpus-calibrands/` dir (the default `-o` target for `pmtiles-corpus mutate`); the durable tilegen output archives (`<output>/<dataset>-<variant>-<commit>.pmtiles`, reproducible) are **spared by a routine clean**. `--archives [--keep N]` prunes those canonical archives to the newest N (default 2) **per (dataset, variant)** - groups are built by *constructing* each known prefix from config, so anything not matching (hand-named files, the toml-contract ocean artifact `data/ocean-tiles.pmtiles`, pre-rename `<dataset>-<commit>` archives) is preserved unconditionally. On ratatoskr projects also wipes the *directories* under `.brokkr/ratatoskr/` (run-N artefact dirs left by failed runs, plus `mock/` dirs from `mock-serve`) but **spares every file at that level, including `gate.db`** - the gate baseline store holds the only copy of the numbers `brokkr.toml` pins by UUID, so it is out of scope like results.db and piners' runs.db. On piners projects removes the `.brokkr/piners/corpus/run-N/` dirs but **spares `runs.db`** (the corpus run store is the source of truth). On dellingr projects removes `.brokkr/dellingr/` (the bench harness's hotpath report and marker FIFO). `--worktrees` purges all persistent benchmark worktrees and (on elivagar) wipes the durable output store wholesale. `--all` is `--worktrees` + `--archives` + `--cargo`; `--dry-run` lists what would go without deleting.
- `dellingr --lua NAME [--bench N|--hotpath N|--alloc N] [--commit REF]` - **[dellingr]** benchmark a registered Lua workload. Builds the `[dellingr] example` cargo example target (bare for `--bench`, `+hotpath` / `+hotpath-alloc` for the profiling modes) and runs it against the workload path, which is resolved from `[dellingr.workloads.*]` and verified against its pinned xxh128 before anything is built. Under `--commit` the harness comes from the worktree but the workload comes from the current tree - deliberately, so a baseline varies the VM and not the script. Rows are filed under the workload name (`brokkr results --command NAME`). See `docs/projects/dellingr.md`.
- `hotpath [--bench N|--hotpath N|--alloc N] [--commit REF] [--target NAME]` - **[sluggrs]** the rendering bench. Builds a cargo example (`--target hotpath` -> the `hotpath` example, filed under the command name `render`; any other `NAME` -> the `NAME_bench` example, filed under `NAME`) and runs it through the standard bench harness. `--hotpath`/`--alloc` add the `hotpath`/`hotpath-alloc` feature; `--bench` builds bare - the uninstrumented walls, the ones worth comparing. With no mode flag it stays `--hotpath 1`, the historical meaning of a bare `brokkr hotpath`; every other measured command would resolve that to `Run`. `--commit` needs no split-tree rule (unlike dellingr): the example and the renderer are both code, so the whole subject comes from the worktree. See `docs/commands/hotpath.md`.
- `pmtiles-stats` - **[elivagar, nidhogg]** PMTiles v3 file statistics (zoom distribution, tile sizes, compression).
- `pmtiles-corpus <sub>` - **[elivagar]** wrap elivagar's corpus namespace (`check`/`bless`/`render-manifest`/`render`/`rings`/`mutate`) - the git-committed output corpus that is the standing baseline. Resolves the archive via the same `[--dataset D] [--variant V] [--commit H | --file P]` resolver as `pmtiles-inspect`; `--corpus` defaults to `corpus/<dataset>` under the build root. `mutate` defaults `-o` to `data/corpus-calibrands/` (cleared by a routine `brokkr clean`). Convenience, never safety: exit codes 0 (pass) / 1 (mismatch) / 2 (refusal) pass through unchanged. See `docs/projects/elivagar.md`.
- `ocean-build [--dry-run]` - **[elivagar]** build the world-ocean pmtiles artifact, wrapping `elivagar ocean-build`. Derives the invocation from `[<host>.tilegen.default].ocean` (shapefile entries → `--ocean` specs, the `.pmtiles` entry → output path); no override flags. See `docs/projects/elivagar.md`.
- `fmt` / `run` / `install` - locked raw-forwarding cargo wrappers (any project; `fmt` and the others honour `disable_toolchain` by riding the global lock's moved-aside window). `install` with no args defaults to `cargo install --path .` - the session-workflow closer; any args replace the default and forward raw.
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

## Session workflow

After a code change, carry it all the way through without checking in first: update the affected markdown (including stale lines adjacent to the change, not only the lines the change touched), run `brokkr check`, commit on master, `brokkr install`. Markdown updates land BEFORE the code commit and ride in the same commit (never a pure-markdown commit). Do not end a turn with "want me to commit?" or "should I update the docs too?".

Note: `git add -A` fails in this repo when `scratch/certifies-smoke/` exists - the smoke script generates a nested git repo there - so add paths explicitly.

## Cross-repo state (nautilus_trader)

Coordination state invisible from either repo alone; migrated from session memory 2026-07-24.

- **Convention-engine port: all engines DONE** (gremlins codepoint ranges, `[style]`, `[header]`, `[[textlint]]` incl. region tracking, `join_wrapped_use`, and the context-window gates, `[manifest]`, `brokkr deps` `workspace_dep`). The remaining work is product, not engine: encode nautilus's *curated* rule set into nautilus's own brokkr.toml (priority #1). The only inexpressible hook is `check_docs_conventions` rule 1 (# Panics/# Errors - needs syn, stays a hand-run hook). Spec: `~/Programs/PRs/work/brokkr-hook-port-spec.md`.
- **Tiered check (`TIERED-CHECK.md`): committed core done, gate GREEN on nautilus** (recorded at c44dd37; the doc's "Continuing this work" section is the handoff). Open items the user is not sure should be done at all: (1) the `[[quarantine]]` ledger reports counts, not membership - an always-too-wide entry or one whose members belong to another issue passes unnoticed; (2) features 6/7 (conditional sweeps, slow-test budget), each with a named re-evaluation criterion in the doc; (3) feature 12 (`requires`), designed-not-built.
- **Nautilus-side follow-ups recorded nowhere else:** `redis::msgbus::serial_tests::` (11 tests) hang on absent Redis - platform-gated but not availability-gated, ~3.7min watchdog burn per gate run until fixed or quarantined; nautilus brokkr.toml's "runs properly serially" comment is falsified by the gate findings and needs a rewrite; pr-backlog.md bundle IDs (B14, B42, B49, B50...) are load-bearing append-only config now that `[[quarantine]]` entries cite them.
- When nautilus reports gate runs, findings usually split into a brokkr defect (fix here) and a nautilus test/config issue (theirs); check TIERED-CHECK.md before adding mechanisms - most questions are already decided there.
