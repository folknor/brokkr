# brokkr

Shared development tooling for pbfhogg, elivagar, nidhogg, litehtml-rs, sluggrs, ratatoskr, piners, dellingr, and many other projects.

## How it works

Invoked as `brokkr` from any project root. Reads `brokkr.toml` for project detection (`project = "foo"`). Commands are gated by project - running a pbfhogg-only command from elivagar's root produces an error.

`brokkr.toml` is looked up in the working directory, or one level up (its immediate parent).

## Detailed docs

These files are not auto-loaded - read them on demand based on what the user asks. Don't `wc` them before reading - just Read them.

They are also compiled into the binary and readable as `brokkr man <topic>` (bare `brokkr man` lists the topics available in the current project). Topics are project-filtered by the same `Visibility` the CLI uses. When editing this list, update `TOPICS` to match.

A doc with 8+ `##` sections is **section-addressed**: `brokkr man conventions script_check` prints that section alone and bare `brokkr man conventions` lists the sections. Slugs derive from the headings (`src/man/sections.rs`), so a doc's `##` structure is its lookup surface - keep one subject per `##`, or it can't be reached.

- `reference/architecture.md` - the full module map: what each source file owns and why. Read before changing a module you have not touched this session.
- `reference/output.md` - operator output channel conventions.
- `docs/brokkr.toml.md` - config core: where it lives, the user-wide layer, `include` composition, top-level shape and host sections, `worktree_keep`, `disable_toolchain`, `[cargo_env]`, `[deps]`, `[bin]`, `[member_check]`, `[lints]`.
- `docs/brokkr.toml.conventions.md` - convention gates: `[gremlins]`, shared path selectors, `[header]`, `[[textlint]]`, `[textlint_preset]`, `[[script_check]]`, `[manifest]`, `[rustdoc]`, `[[fixture_lint]]`, `[[dependency_rule]]`.
- `docs/brokkr.toml.sweeps.md` - `[[check]]` sweeps, the `parallel` lane, `[test]` and its profiles, `[[quarantine]]`.
- `docs/brokkr.toml.elivagar.md` - `[<host>.tilegen.*]` blocks (elivagar).
- `docs/brokkr.toml.datasets.md` - `[<host>.datasets.*]` and the variant-selection CLI flags (pbfhogg/elivagar/nidhogg).
- `docs/brokkr.toml.piners.md` - the `[piners]` config block.
- `docs/commands/check.md` - using `brokkr check`: flags, output, invocation selection, feature projection, the markdown-only shortcut, the convention/clippy/rustdoc phases, `certifies` and exit codes, log lines, sweep selection.
- `docs/commands/check.test-lanes.md` - check's test phase: serial/parallel/process-isolated/nextest/doctest lanes, the coverage audit, diagnostic continuation, feature unification, member-check, dead filters, `BROKKR_TEST_BIN_DIR`.
- `docs/commands/test.md` - `brokkr test`: discovery, matching, `-N`, `--timeout`.
- `docs/commands/check.hygiene.md` - time ceilings and the per-test cap, strays and orphan reaping, the rustc guard.
- `reference/check-internals.md` - how the check/test pipeline is built: invocation selection, harness isolation and the failure roster, direct execution, the prepare plan, journal and execution accounting, diagnostic inventory, watchdog attribution, the stray/orphan reapers, the guard's lease protocol, rustflags sink selection. Read before changing any of them.
- `docs/commands/clippy.md` - `brokkr clippy`.
- `docs/commands/deps.md` - `brokkr deps`.
- `docs/commands/bench.md` - `brokkr bench` (criterion).
- `docs/commands/run.md` - `brokkr run` / `brokkr build` / `brokkr install`.
- `docs/commands/clean.md` - `brokkr clean`.
- `docs/commands/results.md` - `brokkr results`.
- `docs/commands/measure.md` - `--bench`/`--hotpath`/`--alloc`/`--stop`, the sidecar profiler, markers, `brokkr sidecar`.
- `docs/commands/output-channels.md` - where a run's output goes (stdout/stderr/FIFO, results.db vs sidecar.db).
- `docs/commands/visual.md` - litehtml-rs/sluggrs visual commands.
- `docs/commands/hotpath.md` - sluggrs `brokkr hotpath`.
- `docs/commands/sync.md` - ratatoskr `sync` / `mock-serve`.
- `docs/commands/ratatoskr-gate.md` - ratatoskr `--gate` and gate.db.
- `docs/commands/service.md` - ratatoskr `service`.
- `docs/commands/corpus.md` - piners `brokkr corpus`.
- `docs/projects/pbfhogg-vs-elivagar.md` - how the two dispatch layers differ.
- `docs/projects/pbfhogg.md`, `elivagar.md`, `nidhogg.md`, `dellingr.md`, `mogwai.md`, `litehtml.md`, `sluggrs.md`, `ratatoskr.md`, `piners.md` - per-project commands and internals.

## Architecture

Single crate, single binary (plus the `brokkr-rustc-guard` bin). No workspace. The per-file map lives in `reference/architecture.md`; the landmarks:

- `src/main.rs`, `src/main_parts/` - startup and dispatch. `src/cli/` - clap schema, and `visibility.rs`'s `CommandSpec` catalogue, which drives the project gate, help visibility, lock policy, worktree selection and man topics.
- `src/check_cmd/`, `src/test_runner/`, `src/test_cmd/`, `src/build_purpose/` - the check/test/clippy pipeline.
- `src/config.rs`, `src/config_parts/` - config schema, composition, project sections.
- `src/output/` - console and child-process runners; every spawn goes through `process/spawn.rs`'s `ChildPolicy`.
- `src/lock/`, `src/hold.rs`, `src/guard.rs`, `src/stray.rs`, `src/test_orphans.rs` - the global lock, compile capability, rustc guard and reapers.
- `src/measure/`, `src/harness.rs`, `src/db/`, `src/sidecar.rs` - measurement and result stores.
- `src/<project>/` - project-specific commands.

### Project-scoped help

`brokkr --help` lists only the commands that apply to the detected project. A hidden subcommand still parses; dispatch checks its command specification and refuses an inapplicable project before entering its handler.

## Shared commands quick reference

- `check` / `test` - the validation pipeline (see `docs/commands/check.md`). A markdown-only dirty tree runs the prose phases only.
- `clippy`, `deps`, `bench`, `run` / `build` / `install`, `clean`, `results` - see their docs above.
- `man [TOPIC] [SECTION...]` - read the bundled docs.
- `env` - host facts, tool versions, dataset status and digests.
- `wc [threshold]` - tracked `.rs` files over `threshold` lines (default 800).
- `history` - global command history.
- `strays [--kill]`, `guard [--install|--remove]`, `kill [--hard]` - process hygiene (see `docs/commands/check.hygiene.md`).
- `sidecar <uuid>` - sidecar profiler data (see `docs/commands/measure.md`).
- `fmt` - locked `cargo fmt` wrapper.
- Project-specific: `corpus-results`, `dellingr`, `mogwai`, `hotpath`, `pmtiles-stats`, `pmtiles-corpus`, `ocean-build` - see the project docs.

## Conventions

- Operator messages go through `src/output.rs` (prefixes, run-log teeing, quiet behavior); see `reference/output.md`. Use `output::warn` for non-fatal bookkeeping failures and `output::format_duration` for human-facing durations.
- `DevError` variants for structured error handling (no `.unwrap()`).
- `CommandSpec` is the dispatch project gate and also drives visibility, help and worktree selection.
- `cargo_build` refuses to run unless this process holds the brokkr lock.

## Rules

### General rules

- Don't use gremlins! Em-dash, en-dash, strange quotes, whatever - they're all verboten.
- Don't remind the user of the rules. They wrote them, so they know them.
- The user can exempt you from any rule at any time.

### Config contract

**Never break an existing brokkr.toml without asking.** Without explicit user approval, never:

- turn a config that loads today into a load error (retiring a key, a new required table, stricter validation);
- change what an existing key, default, or absent key implies;
- rename or remove a name the config refers to (such as phase names in `skip_phases`).

New keys and tables are fine as long as every existing config behaves identically. When a feature seems to need a break, stop and ask, naming the configs it would affect.

### Bash rules

- Never read or write from `/tmp`. All data lives in the project.
- Never run raw `cargo`, `curl`, `pkill`. Use `brokkr`.

## Document folders

| Folder | Contents | Rule |
|---|---|---|
| `reference/` | How the thing is built and why: architecture, specs, performance records, invariants, protocol contracts | Durable and binding. What it says must be true. |
| `docs/` | How the thing is used: guides, CLI reference, consumer-facing API | Durable and binding. |
| `notes/` | Transient: todos, plans, research, bug reports | No truth guarantee. Nothing durable cites it. |
| `plans/` | Retired | Plans go in `notes/`. |

The dependency direction is one-way: `notes/` may cite `docs/` and `reference/`, never the reverse - not a code comment either, since a comment outlives the note. Root-level convention files (`AGENTS.md`, `README.md`, ...) stay at the root. Avoid citing source line numbers in any of these folders.
