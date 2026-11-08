# brokkr

Shared development tooling for pbfhogg, elivagar, nidhogg, litehtml-rs, sluggrs, ratatoskr, piners, dellingr, and many other projects.

## How it works

Invoked as `brokkr` from any project root. Reads `brokkr.toml` for project detection (`project = "foo"`), looked up in the working directory or its immediate parent. Commands are gated by project: `brokkr --help` lists only the ones that apply, and a hidden subcommand still parses but is refused before its handler runs.

## Detailed docs

These files are not auto-loaded - read them on demand based on what the user asks. Don't `wc` them before reading - just Read them.

The `docs/` files are also compiled into the binary as `brokkr man <topic>` (bare `brokkr man` lists the current project's topics), project-filtered by the CLI's `Visibility`; the `reference/` files are not. When editing this list, update `TOPICS` to match. A doc with 8+ `##` sections is section-addressed (`brokkr man conventions script_check`; bare `brokkr man conventions` lists them), slugs derived from the headings (`src/man/sections.rs`), so keep one subject per `##`.

- `reference/architecture.md` - the full module map. Read before changing a module you have not touched this session.
- `reference/output.md` - operator output channel conventions.
- `docs/brokkr.toml.md` - config core: location, the user-wide layer, `include`, host sections and the top-level keys and tables.
- `docs/brokkr.toml.conventions.md` - convention gates: gremlins, textlint, script checks, manifest, rustdoc, fixture lints, dependency rules.
- `docs/brokkr.toml.bench.md` - script bench config: `[bench.*]`, params and binding, the resolved view, `[bench_params.*]`, `[data]`, `bench_lib`, the `bench` check phase.
- `docs/brokkr.toml.sweeps.md` - `[[check]]` and `[[lint_sweep]]` sweeps, `[test]` profiles, `[[quarantine]]`.
- `docs/brokkr.toml.elivagar.md`, `docs/brokkr.toml.datasets.md`, `docs/brokkr.toml.piners.md` - project config blocks.
- `docs/commands/check.md` - using `brokkr check`.
- `docs/commands/check.test-lanes.md` - check's test phase and its lanes.
- `docs/commands/check.hygiene.md` - time ceilings, `brokkr lock`, strays and orphans, the rustc guard.
- `reference/check-internals.md` - how the check/test pipeline is built. Read before changing it.
- `docs/commands/test.md`, `clippy.md`, `deps.md`, `crit.md`, `run.md` (also `build`, `install`), `clean.md`, `results.md` - those commands.
- `docs/commands/bench.md` - `brokkr bench NAME`: running a script bench, its builtins, what a run stores.
- `docs/commands/measure.md` - `--bench`/`--hotpath`/`--alloc`/`--stop`, the sidecar profiler, `brokkr sidecar`.
- `docs/commands/output-channels.md` - where a run's output goes.
- `docs/commands/library.md` - pbfhogg `brokkr library`.
- `reference/library-internals.md` - how the library harness consumer is built. Read before changing `src/pbfhogg/library/`.
- `reference/bench-store.md` - the store of script-bench runs and the results server wire contract. Read before changing `src/bench/store/`.
- `reference/results-server.md` - how `brokkr-results-server` is built. Read before changing `src/bin/results_server/`. Running it: `docs/results-server.md`.
- `reference/bench-resources.md` - what a script-bench iteration owns: leases, admission, handles, placement. Read before changing `src/reservations/iteration.rs` or `src/bench/resources.rs`.
- `docs/commands/visual.md`, `hotpath.md`, `sync.md`, `ratatoskr-gate.md`, `service.md`, `corpus.md` - project-specific commands.
- `docs/projects/*.md` - per-project commands and internals; `pbfhogg-vs-elivagar.md` contrasts the two dispatch layers.

## Architecture

Single crate: the `brokkr` binary, plus the `brokkr-rustc-guard` and `brokkr-results-server` bins, which include shared sources by path. No workspace. The per-file map lives in `reference/architecture.md`; the landmarks:

- `src/main.rs`, `src/main_parts/` - startup and dispatch. `src/cli/` - clap schema, and `visibility.rs`'s `CommandSpec` catalogue, which drives the project gate, help visibility, lock policy, worktree selection and man topics.
- `src/check_cmd/`, `src/test_runner/`, `src/test_cmd/`, `src/build_purpose/` - the check/test/clippy pipeline.
- `src/config.rs`, `src/config_parts/` - config schema, composition, project sections.
- `src/output/` - console and child-process runners; every spawn goes through `process/spawn.rs`'s `ChildPolicy`.
- `src/lock/`, `src/hold.rs`, `src/guard.rs`, `src/stray.rs`, `src/test_orphans.rs` - the global lock, compile capability, rustc guard and reapers.
- `src/measure/`, `src/harness.rs`, `src/db/`, `src/sidecar.rs` - measurement and result stores; `src/bench/store/` is the per-user store of script-bench runs, served over HTTP by `src/bin/results_server/` and reached by `src/bench/remote.rs`.
- `src/bench/` - script benches: `runner/` runs `brokkr bench NAME`, `lua.rs` is the only seam to the dellingr VM.
- `src/<project>/` - project-specific commands.

## Shared commands quick reference

- `check` / `test` - the validation pipeline. A markdown-only dirty tree runs the prose phases only.
- `bench`, `clippy`, `deps`, `crit`, `run` / `build` / `install`, `clean`, `results` - see their docs above.
- `man [TOPIC] [SECTION...]` - read the bundled docs.
- `env` - host facts, tool versions, datasets, and brokkr's own store sizes.
- `wc [threshold]` - tracked `.rs` files over `threshold` lines (default 800).
- `history` - global command history.
- `lock`, `strays [--kill]`, `guard [--install|--remove]`, `kill [--hard]` - lock status and process hygiene.
- `sidecar <uuid>` - sidecar profiler data.
- `fmt` - locked `cargo fmt` wrapper.
- Project-specific: `corpus-results`, `mogwai`, `hotpath`, `library`, `pmtiles-stats`, `pmtiles-corpus`, `ocean-build` - see the project docs.

## Conventions

- Operator messages go through `src/output.rs` (prefixes, run-log teeing, quiet behavior); see `reference/output.md`. Use `output::warn` for non-fatal bookkeeping failures and `output::format_duration` for human-facing durations.
- `DevError` variants for structured error handling (no `.unwrap()`).
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

### Older brokkr binaries

Never guard against one running, or caveat a gap only one could open. What one left on disk is ordinary state: sweep it if it's big and nameable by construction. Stored data is a different matter too: `brokkr.toml` (the config contract above) and the history in results.db, sidecar.db and runs.db keep reading as they do.

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
