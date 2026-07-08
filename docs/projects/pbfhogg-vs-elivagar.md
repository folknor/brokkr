# pbfhogg vs elivagar: how the two dispatch layers differ

Both projects are map-data benchmark drivers sharing the same harness,
DBs, lock, and worktree machinery. But their dispatch layers
(`src/pbfhogg/dispatch.rs`, `src/elivagar/dispatch.rs`) diverge in several
ways that decide how a run is timed, what it stores, and where its output
goes. This file catalogs those divergences. For the output-channel mechanics
themselves, see `docs/commands/output-channels.md`.

## At a glance

| Aspect | pbfhogg | elivagar |
|---|---|---|
| Dispatch entry | `run_command_with_params` | `run_command` |
| Per-command params | `CommandParams` (io flags, snapshot) | none |
| Build kinds | one: `pbfhogg-cli` main binary | three: MainBinary / Example / NoBuild |
| Bench harness path | uniform: `run_external_ok` | varies by build kind |
| Bench timing source | brokkr external wall-clock | tilegen: self-reported stderr `elapsed_ms`; examples: external wall-clock |
| stderr `key=value` -> results.db | never | tilegen only |
| I/O mode flags | `--direct-io` / `--io-uring` / `--compression` | none |
| Output artifact lifecycle | `promote_artifact` + snapshots | `rename_elivagar_output` + durable output store (regress/bless) |
| Run-specific metadata | - | `locations_on_ways` stderr detection |
| External-tool baselines | none | planetiler (Java), tilemaker (C++) |

## 1. Dispatch entry point and parameters

pbfhogg exposes `run_command_with_params(req, command, osc_seq, extra_params)`
- the extra `CommandParams` carries pbfhogg-only knobs: `direct_io`,
`io_uring`, `compression`, `as_snapshot`/`replace_snapshot`, `index_type`,
`bbox`, `keep_cache`. It runs `resolve_io_flags` (feature+arg injection,
io_uring preflight) and an `--as-snapshot` collision preflight before
building.

elivagar exposes the leaner `run_command(req, command)` - no equivalent
parameter bundle, no I/O-mode flags, no preflight. Its variance comes from
the command's `BuildKind`, not from caller-supplied params.

## 2. Build model

pbfhogg is always one release build of the `pbfhogg-cli` main binary. Every
command is a subcommand of that one binary.

elivagar's `ElivagarCommand::build_config()` returns one of three kinds
(`src/elivagar/commands.rs`):

- **MainBinary** (`tilegen`) - release build of the elivagar binary.
- **Example** (`pmtiles-writer` -> `bench_pmtiles`, `node-store` ->
  `bench_node_store`) - cargo `--example` builds; the example runs its own N
  internal iterations.
- **NoBuild** (`planetiler`, `tilemaker`) - external Java/C++ tools,
  auto-downloaded, no Rust build at all.

## 3. Bench harness path and timing

pbfhogg routes **every** bench command through `run_pbfhogg_wallclock` ->
`run_external_ok`. Timing is brokkr's own best-of-N external wall-clock
(`elapsed_to_ms(&captured.elapsed)`, `types_run.rs:299`). stdout/stderr are
captured and dropped; no stderr kv parsing.

elivagar forks by build kind (`run_elivagar_bench`):

- MainBinary -> `run_elivagar_wallclock` -> `run_external_with_kv_raw`.
  Timing is the subprocess's **self-reported** `elapsed_ms` from stderr
  (`types_run.rs:397-398`), not external wall-clock. stderr `key=value`
  metrics land in results.db.
- Example -> `run_elivagar_internal` -> `run_internal` + `run_captured`.
  The example self-iterates; brokkr times one external invocation via
  external wall-clock (`elapsed_to_ms`, `dispatch.rs:323`) and stores only
  `elapsed_ms` (`kv: vec![]`).
- NoBuild -> external-tool handling, outside this path.

**This is the crux the "do run_pbfhogg_wallclock for elivagar too" idea
targets:** pbfhogg's uniform external-wall-clock, best-of-N,
`run_external_ok` path is simpler and mode-agnostic, whereas elivagar's
tilegen depends on the binary self-reporting `elapsed_ms` on stderr (and
erroring if it's absent). Unifying would mean giving elivagar a
wall-clock path that doesn't require the stderr `elapsed_ms=` contract.

## 4. Output channels into the DBs

Consequence of #3, spelled out per command in
`docs/commands/output-channels.md`:

- **sidecar.db** (`emit_counter`/markers via FIFO): every pbfhogg command and
  elivagar `tilegen`. **Not** elivagar examples (their `run_captured` path
  sets no `BROKKR_MARKER_FIFO`) nor the NoBuild tools.
- **results.db `kv`** (stderr `key=value`): elivagar `tilegen` only. No
  pbfhogg command reads stderr kv; pbfhogg metrics must go out as FIFO
  counters instead.

## 5. I/O mode flags

pbfhogg supports `--direct-io` (adds the `linux-direct-io` feature +
`--direct-io` arg) and `--io-uring` (`linux-io-uring` feature + preflight),
gated per-command by `supports_io_uring()`, plus `--compression`. These
appear in the recorded `cli_args`, so a variant is distinguishable in
results.db by its argv rather than a separate column.

elivagar has no I/O-mode flag surface.

## 6. Output artifact lifecycle

pbfhogg promotes outputs via `promote_artifact` and integrates with the
snapshot graph (`--as-snapshot`, `--replace-snapshot`, dataset variant
selection).

elivagar renames scratch output into place with `rename_elivagar_output`
and maintains the **durable tilegen output store**
(`<output>/<dataset>-<commit>.pmtiles`), the source of truth for `regress`
/`bless` output-regression diffing. A routine `brokkr clean` spares that
store; only `clean --worktrees` reclaims it. See
`docs/projects/elivagar.md`.

## 7. Run-specific metadata

elivagar's tilegen path scans stderr with
`detect_locations_on_ways_stderr` and stamps
`meta.locations_on_ways_detected` into the row metadata. pbfhogg has no
comparable stderr-derived metadata step (its equivalent knobs are explicit
CLI flags recorded in `cli_args`).

## What they share

Same `BenchContext`/`HarnessContext` bootstrap, same `results.db` +
`sidecar.db` at `<db_root>/.brokkr/`, same per-user lock, same worktree
lifecycle for retroactive benchmarking, same run mode (`run_passthrough_timed`,
inherited stdio, no DB, no sidecar) and same `--hotpath`/`--alloc` path
(`run_hotpath_capture`, JSON report into results.db). The divergences above
are all in the bench-mode dispatch, not the shared plumbing.

## Reconciliation candidates (not yet done)

- Give elivagar a pbfhogg-style external-wall-clock bench path so tilegen
  needn't self-report `elapsed_ms` on stderr.
- Or, conversely, let pbfhogg commands opt into stderr-kv metrics
  (`run_external_with_kv_raw`) so per-command counters can reach results.db
  instead of only sidecar.db.
