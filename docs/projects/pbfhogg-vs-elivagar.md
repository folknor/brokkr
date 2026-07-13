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
| Bench harness path | uniform: `run_external_ok` | MainBinary `run_external_ok`; Example `run_internal`; NoBuild external |
| Bench timing source | brokkr external wall-clock | brokkr external wall-clock (tilegen + examples) |
| stderr `key=value` -> results.db | never | never |
| I/O mode flags | `--direct-io` / `--io-uring` / `--compression` | none |
| Output artifact lifecycle | `promote_artifact` + snapshots | `rename_elivagar_output` + durable output store (regress/bless) |
| Run-specific metadata | - | - |
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

- MainBinary -> `run_elivagar_wallclock` -> `run_external_ok`. **Same path
  pbfhogg uses:** timing is brokkr's own best-of-N external wall-clock
  (`elapsed_to_ms`), and stderr is captured and discarded. tilegen emits all
  its metrics as FIFO sidecar counters (elivagar-side 54f9b07), so brokkr no
  longer reads stderr - there is no `elapsed_ms=` contract. Runs are
  distinguished purely by their recorded `cli_args` (the `--locations-on-ways`
  flag lands in the subprocess argv when passed), exactly as pbfhogg does.
- Example -> `run_elivagar_internal` -> `run_internal` + `run_captured`.
  The example self-iterates; brokkr times one external invocation via
  external wall-clock (`elapsed_to_ms`) and stores only `elapsed_ms`
  (`kv: vec![]`).
- NoBuild -> external-tool handling, outside this path.

The tilegen timing source used to be the crux divergence - it depended on the
binary self-reporting `elapsed_ms` on stderr and erroring if it was absent.
That's now gone: tilegen routes through the same `run_external_ok` wall-clock
path as pbfhogg, so both are uniform, best-of-N, and mode-agnostic.

## 4. Output channels into the DBs

Consequence of #3, spelled out per command in
`docs/commands/output-channels.md`:

- **sidecar.db** (`emit_counter`/markers via FIFO): every pbfhogg command and
  elivagar `tilegen`. **Not** elivagar examples (their `run_captured` path
  sets no `BROKKR_MARKER_FIFO`) nor the NoBuild tools.
- **results.db `kv`** (stderr `key=value`): none. Neither pbfhogg nor elivagar
  reads stderr kv into results.db anymore - all runtime metrics go out as FIFO
  counters into sidecar.db instead. (tilegen used to be the sole exception via
  `run_external_with_kv_raw`; it moved to `run_external_ok` + FIFO counters.)

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

Neither bench path derives metadata from stderr. tilegen's old
`detect_locations_on_ways_stderr` -> `meta.locations_on_ways_detected` stamp
was dropped when tilegen moved to `run_external_ok` (it no longer reads
stderr); locations-on-ways is now distinguished by the `--locations-on-ways`
flag in `cli_args`, matching how pbfhogg records its equivalent knobs.
(elivagar's hotpath/alloc path still parses stderr for the stamp, since
`run_hotpath_capture` returns stderr regardless - that's the one place the
detection survives.)

## What they share

Same `BenchContext`/`HarnessContext` bootstrap, same `results.db` +
`sidecar.db` at `<db_root>/.brokkr/`, same per-user lock, same worktree
lifecycle for retroactive benchmarking, same run mode (`run_passthrough_timed`,
inherited stdio, no DB, no sidecar) and same `--hotpath`/`--alloc` path
(`run_hotpath` wrapping `run_hotpath_capture`: the function-level JSON report
into results.db, plus the `/proc` trajectory + FIFO counters into sidecar.db,
so `brokkr sidecar <uuid>` works for hotpath/alloc runs too). The divergences
above are all in the bench-mode dispatch, not the shared plumbing.

## Reconciliation history

- **Done:** tilegen now uses the pbfhogg-style external-wall-clock bench path
  (`run_external_ok`), so it no longer self-reports `elapsed_ms` on stderr. The
  elivagar binary emits its metrics as FIFO sidecar counters instead
  (elivagar-side 54f9b07). This converged the timing source, the stderr->kv
  channel, and the run-specific metadata rows (#3, #4, #7 above).
- **Not done (and no longer needed for tilegen):** letting pbfhogg commands
  opt into stderr-kv metrics. With tilegen off the stderr-kv path, no command
  routes runtime metrics into results.db `kv` - they all use FIFO counters ->
  sidecar.db.
