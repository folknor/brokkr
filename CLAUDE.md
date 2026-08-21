# brokkr

Shared development tooling for pbfhogg, elivagar, nidhogg, litehtml-rs, sluggrs, ratatoskr, piners, dellingr, and many other projects.

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

`review` fans a code review out to fresh AI sessions. Config is `.review.toml` in the repo root: `[archetypes]` (name = priming prompt), `[_defaults].providers`, and `--profile` overrides scoped `[<host>.<provider>.<profile>]`. This repo currently defines the `bugs` archetype against the `codex` provider, with `bugs` and `implement` profiles.

```
echo "Please review the unstaged changes and report your findings." | review bugs --profile bugs
```

- **Piping into `review` is the one sanctioned exception to the no-`|` bash rule above.**
- **Keep the piped prompt to one line.** The archetype prime already tells the session to inspect the current repository state, and each run starts fresh and fetches the code itself. A long prompt that summarises the diff and pre-states where the bugs probably are defeats the point - the session's value is that it reaches its own conclusions, and a reviewer told what to think tends to agree. Say what to look at, not what to find.
- Each run is a fresh session; the printed session ID resumes it via `--session` while the cache is warm.
- `--dry-run` prints the assembled prompt instead of sending it.

## How it works

Invoked as `brokkr` from any project root. Reads `brokkr.toml` for project detection (`project = "foo"`). Commands are gated by project - running a pbfhogg-only command from elivagar's root produces an error.

`brokkr.toml` is looked up in the working directory, or one level up (its immediate parent).

## Detailed docs

These files are not auto-loaded - read them on demand based on what the user asks. Don't `wc` them before reading - just Read them.

They are also compiled into the binary and readable as `brokkr man <topic>` (bare `brokkr man` lists the topics available in the current project). Topics are project-filtered by the same `Visibility` the CLI uses. When editing this list, update `TOPICS` to match.

A doc with 8+ `##` sections is **section-addressed**: `brokkr man config script_check` prints that section alone and bare `brokkr man config` lists the sections. Slugs derive from the headings (`src/man/sections.rs`), so a doc's `##` structure is its lookup surface - keep one subject per `##`, or it can't be reached.

- `docs/brokkr.toml.md` - read if the user asks about config fields, host sections, the `[gremlins]` exclude list, `[[check]]`, `[test]` profiles, `[header]`, `[[textlint]]`, `[[script_check]]`, `[manifest]`, `[bin]`, `[lints]` (formerly `[clippy]`), `worktree_keep`, `[[quarantine]]`, or the user-wide
  `$XDG_CONFIG_HOME/brokkr/brokkr.toml` layer. Schema-universal sections only; project-specific blocks live in their own docs (below).
- `docs/brokkr.toml.elivagar.md` - read if the user asks about `[<host>.tilegen.*]` blocks (the elivagar tilegen contract: ocean inputs, budgets, geometry - `brokkr tilegen` has no override flags). Split out of `brokkr.toml.md` so a non-elivagar tree never sees it.
- `docs/brokkr.toml.datasets.md` - read if the user asks about `[<host>.datasets.*]` (pbf/osc/pmtiles entries) or the variant-selection CLI flags (`--variant`, `--osc-seq`, `--tiles`, `--snapshot`, `--as-snapshot`, `--direct-io`, `--io-uring`, `--compression`, `--locations-on-ways`) - map-data projects only (pbfhogg/elivagar/nidhogg).
- `docs/commands/deps.md` - read if the user asks about `brokkr deps` (any Rust+git repo, not project-gated): the phase model, the `duplicate_version`/`git_dependency`/`path_dependency`/`publish_cycle`/`workspace_dep`/`native_code`/`outdated`/`stale` phases, focus mode, the `ccu --json` shell-out, exit codes, or the planned `advisory` phase.
- `docs/commands/check.md` - read if working on `brokkr check` or `brokkr test`, the gremlins/clippy/test pipeline, sweep selection, profile resolution, libtest filters, the bundled-nextest groundwork (`src/check_cmd/nextest.rs`: the `(binary-id, test)` coverage pair, `MismatchReason` precedence, the universe-listing rule), or the `BROKKR_TEST_BIN_DIR` contract.
- `docs/commands/bench.md` - read if the user asks about `brokkr bench` (any Rust repo with criterion benches, not project-gated): the measure/compare split, baseline naming and the dirty-tree refusal, the environment stamp that gates `--compare`, why baselines live under `.brokkr/bench/` via `CRITERION_HOME`, or why criterion and iai targets can't be told apart at discovery.
- `docs/commands/run.md` - read if the user asks about `brokkr run` or `brokkr install` (any Rust repo, not project-gated): cargo-metadata target discovery, the bare-is-an-index form, the `--` argv pre-pass, package selection for install, or profile precedence.
- `docs/commands/clean.md` - read if the user asks about `brokkr clean` - the constructed-name rule, what a routine clean removes per project, the permanently-out-of-scope measurement stores, `--cargo`/`--archives`/`--worktrees`.
- `docs/commands/results.md` - read if the user asks about `brokkr results` - filters, `--grep`/`--grep-v` over the whole invocation, single-run lookup, or `--compare`'s pairing key and what it strips.
- `docs/commands/clippy.md` - read if working on `brokkr clippy` - the investigative single-phase clippy runner (ad-hoc `-p` + real `--all-features`, or `--sweep NAME` to replay one `[[check]]` entry), the ad-hoc env-union rule and `--env` overrides, or how it reuses `check`'s clippy pipeline (`src/check_cmd/phase.rs::cmd_clippy`).
- `docs/commands/visual.md` - read if the project is litehtml-rs or sluggrs and the user asks about `visual`, `list`, `approve`, `report`, `visual-status`, `prepare`, `html-extract`, or `outline`.
- `docs/commands/hotpath.md` - read if the project is sluggrs and the user asks about `brokkr hotpath` - the rendering bench: the mode->feature mapping, the `--target` double naming (`hotpath` -> filed as `render`; anything else -> `NAME_bench`), or `--commit` (no split-tree rule, unlike dellingr).
- `docs/commands/sync.md` - read if the project is ratatoskr and the user asks about `sync` or `mock-serve`: sæhrimnir orchestration, readiness sentinels, endpoint env-var export, marker FIFOs, and the bench path's `--commit` split-tree rule (dellingr's rule, not sluggrs').
- `docs/commands/ratatoskr-gate.md` - read if the project is ratatoskr and the user asks about `--gate`, the `--gate all` cohort sweep, `--as-baseline`, `[ratatoskr.gate.*]`, baseline pinning per hostname, gate.db, or sync bench regression thresholds.
- `docs/commands/service.md` - read if the project is ratatoskr and the user asks about `service`. One command in the bare-is-an-index shape: bare lists, `<SCRIPT>` runs one (a directory runs that cohort), `--all` runs every discovered script against one shared build. Replaced the former `service-test`/`service-suite`/`service-list` triple. Covers lua VM, frontmatter, ceiling, artefact layout, fixture lifecycle.
- `docs/commands/corpus.md` - read if the project is piners and the user asks about `brokkr corpus` - the parity-corpus runner, the `pins.toml`/keyword registry, probe selection, xxh128 verification, the expected-disposition gate, reseed/bless, or exit codes.
- `docs/commands/lint-corpus.md` - read if the project is piners and the user asks about `brokkr lint-corpus` / `lint-results` - the differential-lint corpus, the `lints.toml` registry, the `(line,col,severity)` diff and dispositions, `--reanchor`, `--bless`, or `[piners.lint]`.
- `docs/brokkr.toml.piners.md` - read if the user asks about the `[piners]` config block (`corpus_root`, `registry_dir`, `feeds`, `harness`).
- `docs/commands/measure.md` - read if the user asks about `--bench`, `--hotpath`, `--alloc`, `--stop`, the sidecar profiler, the marker FIFO, `BenchHarness`, hotpath JSON contract, or `brokkr sidecar` queries. Also covers the marker/counter name-interpretation rules per query view and how `brokkr sidecar` renders its JSONL/`--human` tables.
- `docs/commands/output-channels.md` - read if the user asks where a run's output goes - stdout vs stderr `key=value` vs FIFO markers/counters, which lands in `results.db` (`brokkr results`) vs `sidecar.db` (`brokkr sidecar`), the per-harness-path capture matrix (`run_external_ok`/`run_external_with_kv_raw`/`run_internal`/`run_passthrough_timed`), or the per-command table for pbfhogg + elivagar.
- `docs/projects/pbfhogg-vs-elivagar.md` - read if the user asks how pbfhogg and elivagar's dispatch layers differ - build kinds, bench harness path, timing source (external wall-clock vs self-reported stderr `elapsed_ms`), I/O-mode flags, output-artifact lifecycle, or which output channels each command feeds.
- `docs/projects/piners.md` - read if the project is piners and the user asks about the harness NDJSON/manifest contracts, the `trade_diff` shape, the `runs.db` corpus run store and its schema, or the `brokkr corpus-results` query surface (piners-only).
- `docs/projects/pbfhogg.md` - read if working on pbfhogg-specific commands, verify subcommands, snapshot graph, OSC parser, io_uring/direct-io constraints, or the download command.
- `docs/projects/elivagar.md` - read if working on elivagar-specific commands, incl. `regress` (two-explicit-archive output-regression diff; no baseline registry), `pmtiles-corpus` (the git-committed output corpus, the standing gate), `ocean-build`, and the durable tilegen output store.
- `docs/projects/nidhogg.md` - read if working on nidhogg-specific commands, server lifecycle, or the API client.
- `docs/projects/dellingr.md` - read if the project is dellingr and the user asks about `brokkr dellingr` - the `[dellingr]` config block, the xxh128 workload pin and drift refusal, the mode->feature mapping, the two-tree `--commit` rule, the harness argv + sidecar marker contract, or why the `dataset` column is empty.
- `docs/projects/mogwai.md` - read if the project is mogwai and the user asks about `brokkr mogwai` - the argv-shaped vs harness-shaped surface split, the `[mogwai]`/`[mogwai.targets.*]` registry (targets + feature shapes only, no workloads), why `features` lives in the registry, why there is no workload registry any more and what was removed with it, or `[<host>.datasets.*]` plain-file/directory pinning (xxh128-not-sha256).
- `docs/projects/litehtml.md` - read if working on litehtml/sluggrs internals (modules, fixture preprocessing, Node.js scripts).
- `docs/projects/ratatoskr.md` - read if working on the ratatoskr harness model, sæhrimnir contract, fixture resolution, lua test runtime, or artefact layout.

## Architecture

Single crate, single binary. No workspace.

### Source layout

- `src/main.rs` - `main()`, command dispatch, `run_measured()`, `resolve_mode()`
- `src/cli/` - CLI definition (clap derive), split into `schema.rs` (`Cli`, `Command` incl. `Command::Deps` and all measurable commands, `ModeArgs`, `PbfArgs`, `VerifyCommand`, `Command::as_pbfhogg()`) and `validation.rs` (clap value parsers). All commands are top-level - no subcommand enums for litehtml/sluggrs. `visibility.rs` holds `TABLE`, the subcommand-name -> applicable-projects map that hides irrelevant commands from `--help` (see below); the three files are `include!`d into `src/cli.rs`, so they carry `//` comments, not `//!`

### Project-scoped help

`brokkr --help` lists only the commands that apply to the detected project. Hidden is not disabled: a hidden subcommand still parses and reaches its handler.

- `src/bench_cmd/` - `brokkr bench`, the criterion runner (any Rust repo, not project-gated): `mod.rs` (the measure/compare split, baseline naming, the `CRITERION_HOME` store under `.brokkr/bench/`), `discover.rs` (bench targets from cargo metadata; owning package derived, never supplied), `stamp.rs` (the build-environment record and the shared-fields-only difference rule that gates `--compare`). See `docs/commands/bench.md`.
- `src/cargo_filter.rs` - Formatter primitives (`ClippyDiagnostic`, `ClippyParse`) plus the legacy text-output parser still used as a fallback by the test-phase build-error path. See the module header for why the JSON path replaced text scraping
- `src/cargo_json.rs` - Parser for cargo's `--message-format=json` diagnostics (`DiagnosticEvent`, `parse_cargo_diagnostics`), feeding check's text renderer. The old NDJSON `CheckEvent` output mode was removed (3ff8291); `check --json` now emits a single schema-versioned summary trailer built in `src/check_cmd/phase.rs` (`CheckSummary`)
- `src/gremlins.rs` - Gremlin detector for `brokkr check`. Scans `.rs`/`.toml`/`.md`/`.js`/`.sh` files (tracked + untracked-not-gitignored) for invisible/deceptive Unicode. Exposes `tracked_files()` (shared file walk) and `CodepointSet` (allow/ban singletons + ranges)
- `src/header.rs` - `[header]` check: required file header with a current-year (`{year}`) requirement via libc `gmtime`. Ported from `check_copyright_year`
- `src/textlint.rs` - `[[textlint]]` engine: declarative forbid-a-regex-on-a-line rules with bounded predicates (`allow_marker`, `except`, `in_toml_section`, `table_row_only`). The generic engine for grep-style convention hooks
- `src/rustflags.rs` - Where a build's extra rustc flags actually come from, and how to add to them without discarding them. Cargo picks *one* rustflags source (encoded env > env > matching `target.*` > `build.rustflags`), so exporting `RUSTFLAGS` to add one `-A` silently drops the project's `-Dwarnings` and link flags. Inspects the whole config chain (build-root ancestors + `$CARGO_HOME`), including a small host-`cfg()` evaluator, and returns the live layer as a `Sink`; injection at a config layer goes through `--config`, where cargo's array merge puts brokkr's entry last. An unrecognised `cfg(...)` resolves to the inert direction, never the destructive one. Used by `check`'s test phase to carry `[lints] allow`
- `src/globs.rs` - `globset` wrapper for the path-glob lists shared by `header`/`textlint`
- `src/scope.rs` - Scope + limit helpers. `changed_files()` computes files modified on the current branch via git merge-base; `partition()` sorts diagnostics scoped-first; `format_trailer()` builds the overflow summary
- `src/measure.rs` - `MeasureMode` (Run/Bench/Hotpath/Alloc), `MeasureRequest`, `CommandContext`
- `src/{pbfhogg,elivagar,nidhogg}/dispatch.rs` - Per-project dispatch (split from the old unified `src/dispatch.rs` in 0313f74). Pbfhogg exposes `run_command_with_params()`; elivagar and nidhogg expose `run_command()`. Pbfhogg and elivagar use `BenchContext` for build+harness; nidhogg delegates to per-module functions
- `src/pbfhogg/commands.rs` - `PbfhoggCommand` enum, single source of truth for argument construction
- `src/elivagar/commands.rs` - `ElivagarCommand` enum (Tilegen, PmtilesWriter, NodeStore, Planetiler, Tilemaker)
- `src/context.rs` - `HarnessContext`, `BenchContext`, bootstrap helpers, worktree lifecycle
- `src/resolve.rs` - Path resolution helpers (PBF, OSC, bbox, data dirs, results DB)
- `src/project.rs` - `Project` enum (Pbfhogg/Elivagar/Nidhogg/Litehtml/Sluggrs/Ratatoskr/Piners), `detect()`, `require()` gating
- `src/man.rs` + `src/man/render.rs` + `src/man/sections.rs` - `brokkr man`: the `TOPICS` table (name, summary, `include_str!`'d markdown, `Visibility`), the heading-slug section addresser (`sections.rs` - fence-aware `##`/`###` scan, byte ranges, exact/prefix/substring resolution), and a `pulldown_cmark` -> ANSI terminal renderer (headings, GFM tables, alerts, footnotes, nested lists; links keep their text and drop the URL). Renderer ported from mogwai's `mogwai-server/src/man/render.rs`
- `src/artefacts.rs` - `ArtefactDir`: per-run `<parent>/<test_id>/run-N/` allocator with preserve-on-failure semantics. Shared by ratatoskr (`.brokkr/ratatoskr`) and piners (`.brokkr/piners`)
- `src/config_parts/user.rs` - the user-wide `brokkr.toml` layer (XDG-resolved, `BROKKR_USER_CONFIG` overrides): `[[textlint]]`/`[textlint_preset.*]`/`[[script_check]]` only, everything else rejected by name. Folded in at `project::detect` via `DevConfig::apply_user_layer`, never inside `config::load` - so parsing a config file yields that file alone. User entries first, project entries after; a project entry shadows a user one of the same `name`. A tree with no `brokkr.toml` at all still gets the layer: `check`'s no-detection branch loads it directly, so its textlint/script_check phases run from the user entries alone
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
- `src/worktree.rs` - Persistent git worktrees for retroactive benchmarking. `remove_one` is the shared git-then-filesystem removal used by both `purge_all` and retention eviction; `list` must be given the **build** root (it derives both the search dir and the name prefix from it)
- `src/worktree_record.rs` - `.brokkr/worktrees.toml`: last-used bookkeeping and LRU retention for `--commit` worktrees (`worktree_keep`, default 6). Hooked into `context::with_worktree` rather than a command, since `--commit` spans dellingr/sluggrs/pbfhogg/ratatoskr/bench. Evicts only when cutting a new worktree, never evicts a dirty one, skips-and-reports on failure. A growth damper, not a bound - the count is per project
- `src/history.rs` - `HistoryDb` - global command history at `$XDG_DATA_HOME/brokkr/history.db`
- `src/deps/` - `brokkr deps` dependency audit (any Rust+git repo, not project-gated). Phase-based like `check`: `mod.rs` (`DepsEvent` enum, `run()`, cargo-metadata deserializer, text + NDJSON renderers), `duplicate_version.rs` (blame-aware duplicate detection), `git_dependency.rs`, `path_dependency.rs`, `publish_cycle.rs` (publication-cycle detection over declared manifests), `workspace_dep.rs`, `native_code.rs`, `ccu.rs` (`ccu --json` shell-out feeding the outdated + stale phases), `focus.rs` (`brokkr deps <pkg>` chain trace). See `docs/commands/deps.md`.

### Project-specific modules

- `src/pbfhogg/` - benchmarks, verify (11 commands + all), download. See `docs/projects/pbfhogg.md`.
- `src/osc.rs` - Minimal `.osc` / `.osc.gz` reader for verify-side delta analysis. See module header and `docs/projects/pbfhogg.md`.
- `src/profile.rs` - Validation profile resolver for `[test.profiles.*]`. See module header and `docs/commands/check.md`.
- `src/elivagar/` - benchmarks, verify, download-ocean, hotpath, `compare_tiles.rs` (the native lenient per-layer sampling census; tolerant decode, no verdict), `regress/` (the native two-explicit-archive semantic diff; no comparability gate - regress is the tier-3 attribution instrument, comparability is the caller's job), `corpus/` (the native `pmtiles-corpus` gate - brokkr owns the adjudication; see below) and `ocean_build.rs` (`ocean-build`, derives the invocation from `[<host>.tilegen.default].ocean`). See `docs/projects/elivagar.md`.
- `src/elivagar/corpus/` + `src/elivagar/eliv.rs` - the corpus gate: brokkr owns the digest fold, gating policy, verdicts, SVG render core and calibration, reading archives in-process through the linked elivagar crate (`eliv.rs` is that seam). Modules: `canonical` (the FROZEN streaming tile hash), `digest`, `diff`, `contract`, `manifest`, `render`, `style`, `mutate`, `fixture`. See `docs/projects/elivagar.md`.
- `src/elivagar/regress/` - the native two-archive semantic diff: `engine` (three-pass raw/canonical/detail), `prepared`, `compare`, `pairing`, `geometry` (exact-integer bbox/Hausdorff/KD-tree), `report`, `overlay`. Pass 2 calls `corpus::canonical`'s hash, so regress and the gate share one definition of "the same tile".
- `src/nidhogg/` - server lifecycle, ingest, update, query, geocode, benchmarks, verify. See `docs/projects/nidhogg.md`.
- `src/litehtml/` - 4 modules: visual reference testing. See `docs/projects/litehtml.md`.
- `src/ratatoskr/` - harness orchestration (`saehrimnir.rs`, `sync.rs`, `cmd.rs`, `discover.rs`). See `docs/projects/ratatoskr.md`.
- `src/dellingr/` - `brokkr dellingr`, the Lua-workload bench. `workload.rs` (the `[dellingr.workloads.*]` registry: `--lua NAME` -> absolute path, xxh128 verified via `preflight::verify_file_hash`; resolution is anchored to `project_root` so a `--commit` run takes the harness from the worktree and the workload from the current tree), `cmd.rs` (build the `example` target with mode-dependent features, then `run_external` for `--bench` or `run_hotpath_capture` for `--hotpath`/`--alloc`). Rows are filed under the workload name with an empty `input_file`. See `docs/projects/dellingr.md`.
- `src/mogwai/` - `brokkr mogwai`. Two surface kinds, one row shape, no layers: argv-shaped surfaces go through the registered bin and need no registration (`brokkr mogwai -- gen ...`), harness-shaped ones resolve a name against `[mogwai.targets.*]` to a cargo example plus the features it must be built with (`targets.rs`). Features live in the registry because `--hotpath`/`--alloc` are inert without them - that is why the predecessor recorded profile-less rows; a call-site `-F` appends rather than replaces. `cmd.rs` branches on mode - `run_hotpath` + `run_hotpath_capture` for `--hotpath`/`--alloc` (the report is a file the child is told about, so `run_external` would record a row with no profile in it), `run_external` otherwise - filing rows under the target (or bin) name with the invocation captured verbatim in `cli_args`/`brokkr_args` - pairing is a query (`--grep` selects an arm, including one defined by an absent flag), not a name lookup. The `[mogwai.workloads.*]` registry, `meta.timing`, `meta.identity_counters` and `[<host>.corpus.*]` were removed wholesale (see the doc's removal table); out-of-git inputs are `[<host>.datasets.*]` with a plain `path`/`xxh128`. See `docs/projects/mogwai.md`.
- `src/piners/` - `brokkr corpus` parity-corpus runner: `registry.rs` (pins.toml + xxh128 verification), `pins_write.rs` (comment-preserving writer via `toml_edit`), `select.rs`, `manifest.rs`, `report.rs` (NDJSON parse/render), `cmd.rs`, `corpus_db/` (the `runs.db` store, mirroring `src/db`), `corpus_query.rs`. See `docs/commands/corpus.md` and `docs/projects/piners.md`.
- `src/piners/lint/` - `brokkr lint-corpus` / `lint-results`: `mod.rs` (`Severity`/`DiagKey`/`DiagSet`/`ProbeResult`), `registry.rs` (`lints.toml` + the TV anchor), `select.rs`, `validators.rs` (both JSON parsers, normalized to `(line,col,severity)`), `diff.rs` (disposition classifier), `lints_write.rs`, `cmd.rs`, `db.rs`, `query.rs`. See `docs/commands/lint-corpus.md`.
- `scripts/litehtml-prepare/` - Node.js fixture preprocessing (cheerio + pngjs).

## Shared commands quick reference

For details, read the linked docs.

- `check` / `test` - validation pipeline. Its `publish_cycle` phase needs no config and ignores `-p`: it shares `deps`' cycle detection so a publication cycle fails `check` too. See `docs/commands/check.md`.
- `bench [TARGET] [--commit REF] [--compare A B] [--baselines] [-- ARGS...]` - the criterion runner (any Rust repo, not project-gated). Measurement and comparison are separate verbs: a run saves a baseline named for the commit, `--compare A B` diffs two stored baselines without sampling. Baselines live under `.brokkr/bench/` (`CRITERION_HOME`) because they are results, not artifacts. `-p` derives from the target's owning package, which makes the one-crate-at-a-time link rule structural. A stamp of toolchain/CPU/rustflags gates `--compare`. See `docs/commands/bench.md`.
- `clippy` - investigative single-phase clippy runner (not project-gated): ad-hoc `-p` + feature flags, or `--sweep NAME` to replay one `[[check]]` entry. See `docs/commands/clippy.md`.
- `deps` - dependency audit (any Rust+git repo, not project-gated): duplicate versions with blame, git/path deps, publication cycles, unused workspace deps, plus informational native-code and staleness. `brokkr deps <pkg>` is focus mode. Exit 1 on offline findings. See `docs/commands/deps.md`.
- `man [TOPIC] [SECTION...] [--full]` - read the bundled `docs/**.md` (rendered markdown->ANSI; plain text when piped). Bare lists the detected project's topics; a long topic lists its sections; `brokkr man check gremlins textlint` reads several, in document order. Section names match exactly, then by prefix, then as a substring. Works with no `brokkr.toml`.
- `env` - hostname, kernel, governor, memory, drives, tool versions, dataset status. A `[<host>.datasets.*]` entry may be a plain out-of-git input (`path` + `xxh128`) rather than the pbf/osc/pmtiles variant shape; with no `xxh128` it prints the digest computed from the path, which is how such a registration gets its digest (brokkr hashes xxh128; delivery manifests carry sha256, so there is nothing to transcribe). A digest that cannot be computed reports why, not a bare `error`. Also prints a `user cfg:` line: the resolved user-wide config path and what it contributed. `path` may be a DIRECTORY - it digests as the sorted fold of `<relpath>\0<file digest>` over the tree (`preflight::compute_xxh128_tree`), since a delivery is often a directory the consuming CLI takes whole.
- `wc [threshold]` - list tracked `.rs` files with more than `threshold` lines (default 800), largest first. Works in any project.
- `results` - query the results database (`.brokkr/results.db`): bare table, `--commit`/`--command`/`--mode`/`--dataset`/`--meta`/`--env` filters, `--grep`/`--grep-v` over the whole invocation (argv plus captured env - the only way to select an env-gated arm, or an arm defined by an *absent* flag), `<uuid>` for one run, `--compare A B` for a paired delta. Uniform everywhere except litehtml. See `docs/commands/results.md`.
  `--compare` also emits a `counters:` line for stderr counters that moved between the two sides - the context that turns "12% faster" into "12% faster on 8% fewer cells". Reported, never fatal: a gate that fires on the first legitimate win earns a bypass flag and then gets passed out of habit. `meta.`/`env.`/`prev.` pairs are excluded as provenance.
- `corpus-results` - **[piners]** query the corpus run store (`.brokkr/piners/corpus/runs.db`); the query sibling of `corpus`. See `docs/projects/piners.md`.
- `lint-corpus` / `lint-results` - **[piners]** the differential-lint corpus (piners vs pine-lint offline, gated on a pinned agreement disposition). See `docs/commands/lint-corpus.md`.
- `clean [--worktrees] [--cargo [PKG]] [--archives [--keep N]] [--all] [--dry-run]` - remove scratch/temp files. Removes only what brokkr created, identified by a brokkr-designated directory or a *constructed* canonical name (never by parsing a filename back); the measurement stores (results.db/sidecar.db/history.db, piners' `runs.db`, ratatoskr's `gate.db`, `bench`'s `.brokkr/bench/` baselines) are permanently out of scope. `--cargo` is the fix for stale-incremental linker failures. See `docs/commands/clean.md`.
- `dellingr --lua NAME [MODE] [--commit REF]` - **[dellingr]** benchmark a hash-pinned Lua workload through the `[dellingr] example` target. `--commit` takes the harness from the worktree and the workload from the current tree, so a baseline varies the VM and not the script. See `docs/projects/dellingr.md`.
- `mogwai [TARGET] [MODE] [-- ARGS...]` - **[mogwai]** benchmark a mogwai surface. Bare lists both kinds; `-- <args>` with no target benches the shipped bin (argv-shaped surfaces need no registration); a name resolves `[mogwai.targets.*]` to a cargo example plus its features (harness-shaped surfaces have no command line, so the harness is the addressable thing). The registry holds targets and inputs, never invocations - pbfhogg's model. See `docs/projects/mogwai.md`.
- `hotpath [MODE] [--commit REF] [--target NAME]` - **[sluggrs]** the rendering bench (a cargo example through the standard harness). Bare stays `--hotpath 1`, unlike every other measured command. See `docs/commands/hotpath.md`.
- `pmtiles-stats` - **[elivagar, nidhogg]** PMTiles v3 file statistics (zoom distribution, tile sizes, compression).
- `pmtiles-corpus <sub>` - **[elivagar]** the git-committed output corpus that is the standing baseline (`check`/`bless`/`render`/`rings`/`mutate`). Convenience, never safety: exit codes 0/1/2 pass through unchanged. See `docs/projects/elivagar.md`.
- `ocean-build [--dry-run]` - **[elivagar]** build the world-ocean pmtiles artifact; the invocation derives from `[<host>.tilegen.default].ocean`, with no override flags. See `docs/projects/elivagar.md`.
- `fmt` - locked raw-forwarding `cargo fmt` wrapper (any project; honours `disable_toolchain` by riding the global lock's moved-aside window).
- `run [NAME] [--debug|--release] [-- ARGS...]` / `install [--debug|--release]` - metadata-driven runnable commands (any project, no config required). `run` discovers every workspace bin + example target from `cargo metadata` and runs NAME; bare form runs `[bin] default` or the sole runnable and lists candidates otherwise (bare-is-an-index). `install` runs `cargo install --path <pkg dir>` for the `[bin] install` packages, or the sole bin-carrying package - the session-workflow closer. Profile: `--debug`/`--release` > `[bin] debug` > release (both). Same lock + `disable_toolchain` window as the build paths. See `docs/commands/run.md`.
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

### Memory rules

Do not use your Memory functionality. Do not read, write, or update memories. Do not suggest saving things to memory. Durable context belongs in CLAUDE.md or the relevant docs.

## Document folders

The standing layout, across every project. Three live folders plus one retired,
split by durability first, subject second.

| Folder | Contents | Rule |
|---|---|---|
| `reference/` | Durable in-repo reference for anyone working on or with the code - how the thing is built and why: `architecture.md`, `technical-implementation-spec.md`, `performance.md` (the durable record of measured numbers over time), invariants, protocol contracts | Citable from source as a source of truth. What it says must be true. |
| `docs/` | Durable in-repo documentation of how the thing is used - guides, CLI reference, the consumer-facing API surface. Sometimes exposed as a hand-edited VitePress gh-pages site | Same must-be-true rule. |
| `notes/` | Transient - work items (`todo.md`), future plans, hypotheticals, bug reports, research, analysis. Things that will die | No truth guarantee. Nothing durable cites it. |
| `plans/` | Retired | Plan documents are transient: they go in `notes/`. |

`reference/` and `docs/` are both durable and both binding. The difference is
subject, not audience: `reference/` covers how the thing is built and why - what
you need in order to change it safely - while `docs/` covers how it is used. A
developer or library consumer reads both. Where a project publishes a site,
`docs/` is what gets published; the folder means the same thing either way.
`notes/` is neither durable nor binding, which is the whole point of keeping it
separate: a document that may be wrong must not sit where a document that must
be right is expected.

The dependency direction is therefore one-way. `notes/` may cite `docs/` and
`reference/`; nothing durable may cite `notes/` - not a code comment, not
`docs/`, not `reference/`. A code comment must carry its full context, because
it outlives the note.

**Root-level convention files are exempt.** `AGENTS.md`, `CLAUDE.md`,
`README.md`, `LICENSE`, `CHANGELOG.md` and their kin are found by tooling and by
convention at the repository root, and stay there. These folders govern
documents we chose where to put, not files whose location is dictated.

In `notes/`, `docs/` and `reference/` alike, avoid citing source line numbers -
they drift fast.
