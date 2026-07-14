# Output channels: how brokkr captures a run's output

When brokkr runs a project binary under a measurement mode, the binary's
output can travel to the operator's terminal, to the tracked results DB, to
the gitignored sidecar DB, or nowhere at all. Which of these happens depends
entirely on **which harness path** the command's dispatch chose - the project
binary itself never knows what mode it runs under. This file is the map.

## The two databases

- `.brokkr/results.db` - **tracked in git**, kept small. One row per bench
  run: `command`, `elapsed_ms`, `commit`, `input`, and a `kv` column of
  arbitrary `key=value` metrics. Queried with `brokkr results`. Two things
  brokkr records itself rather than receiving over a transport channel: the
  per-iteration walls of a `--bench N` run (`run_iterations`, in execution
  order) and the `prev.*` kv pairs naming the previously-recorded run. Neither
  comes from the binary - see `docs/commands/measure.md`.
- `.brokkr/sidecar.db` - **gitignored** (split out of results.db in schema
  v8->v9). Per-run `/proc` sample trajectories, phase markers, and
  application counters. Queried with `brokkr sidecar <uuid>`.

Both live under `<db_root>/.brokkr/` and are resolved identically for every
project by `BenchHarness::new_with_lock` (`src/harness_mod/types_run.rs`).
See `docs/commands/measure.md` for the sidecar's sampling/marker protocol.

## The three transport channels

A running binary can hand data to brokkr three ways:

1. **stdout** - human-readable prose. Either inherited (printed straight to
   the terminal) or captured (drained into a buffer and dropped). Never
   parsed, never stored.
2. **stderr `key=value` lines** - structured metrics. Parsed *only* on the
   kv harness path (`parse_kv_lines`, `src/harness_mod/format_sidecar.rs`).
   A `elapsed_ms=` (or its alias `total_ms=`) line is **mandatory** on that
   path - its absence errors the whole run; if both appear, `elapsed_ms`
   wins. Every other line's value is type-inferred in order: `i64` -> finite
   `f64` -> text (a non-finite float like `NaN`/`inf` falls back to text).
   Those pairs land in the results.db `kv` column and the `[result]` line.
   This is the subprocess's **self-reported** timing, not brokkr's wall-clock
   (the `run_external_ok` path uses external best-of-N wall-clock instead).
3. **FIFO markers + counters** - via `BROKKR_MARKER_FIFO`. The target writes
   `<us> name` (marker) or `<us> @name=value` (counter, value must parse as
   `i64`). Drained by `SidecarFifo::drain` (`src/sidecar.rs`) into
   sidecar.db. In pbfhogg these are emitted by `src/debug.rs`
   (`emit_marker`, `emit_counter`, `emit_mallinfo2`); any project binary can
   emit them by writing that line format to the FIFO. If the env var is
   absent (no sidecar attached), emission is a silent no-op.

## Derived fields on the `[result]` line

`format_result_line` (`src/harness_mod/format_sidecar.rs`) adds fields brokkr
computes itself, on top of the raw kv pairs:

- `read_mbs` - appears whenever the input size is known (`config.input_mb`,
  resolved from the dataset config) and `elapsed_ms > 0`: `input_mb / secs`.
- `write_mbs` - appears additionally when the run reported an `output_bytes`
  kv pair (so this one *does* depend on the binary emitting it): computed as
  `output_bytes/1e6 / secs`.
- `samples`/`min_ms`/`p50_ms`/`p95_ms`/`max_ms` - only for `run_distribution`
  results.

So a metric can reach the `[result]` line three ways: a raw stderr kv pair, a
brokkr-derived throughput field, or (for `write_mbs`) a kv pair that feeds a
derived one. None of these apply on the no-kv `run_external_ok` path except
`read_mbs`, which only needs the input size and brokkr's own timing.

## The harness paths and what each captures

| Harness fn | stdout | stderr kv -> results.db | FIFO -> sidecar.db | Used by |
|---|---|---|---|---|
| `run_passthrough_timed` | inherited (shown) | no | **no** (no FIFO) | all `--` run mode (no `--bench`) |
| `run_external_ok` | captured, dropped | no | yes | every pbfhogg bench command + elivagar `tilegen` |
| `run_external_with_kv_raw` | captured, dropped | **yes** | yes | elivagar `self` micro-bench; pbfhogg read/write/merge micro-benches (via `run_external_with_kv`) |
| `run_internal` (+ `run_captured`) | captured, dropped | no | **no** (no FIFO) | elivagar example benches |
| `run_hotpath_capture` (stored via `run_hotpath`) | captured | via JSON report | yes | `--hotpath` / `--alloc`, all projects |

Two consequences worth internalizing:

- **Run mode (no `--bench`) stores nothing and attaches no sidecar.** stdio
  is inherited, so the binary's own prose reaches the terminal verbatim, and
  `emit_counter` no-ops because `BROKKR_MARKER_FIFO` is unset.
- In every captured path, the binary's stdout prose is **swallowed** - brokkr
  prints its own `[result]` line instead. To survive bench mode, data must
  go out as a FIFO counter or (on the kv path) an stderr `key=value` line.

## Per-command table

`sidecar` = `emit_counter`/`emit_marker` reach sidecar.db (`brokkr sidecar`).
`kv` = stderr `key=value` reaches results.db (`brokkr results`). Both columns
describe **bench mode** (`--bench N`); run mode stores nothing for any command.

### pbfhogg

All measurable pbfhogg commands share one dispatch path
(`run_pbfhogg_wallclock` -> `run_external_ok`), so the answer is uniform:
sidecar **yes**, stderr-kv **no**. `elapsed_ms` in the result is brokkr's own
best-of-N wall-clock, not self-reported.

| Command | Bench path | sidecar | stderr kv |
|---|---|---|---|
| inspect | run_external_ok | yes | no |
| check-refs | run_external_ok | yes | no |
| check-ids | run_external_ok | yes | no |
| sort | run_external_ok | yes | no |
| cat | run_external_ok | yes | no |
| tags-filter | run_external_ok | yes | no |
| getid | run_external_ok | yes | no |
| getparents | run_external_ok | yes | no |
| renumber | run_external_ok | yes | no |
| merge-changes | run_external_ok | yes | no |
| apply-changes | run_external_ok | yes | no |
| add-locations-to-ways | run_external_ok | yes | no |
| time-filter | run_external_ok | yes | no |
| diff | run_external_ok | yes | no |
| build-geocode-index | run_external_ok | yes | no |
| extract | run_external_ok | yes | no |
| multi-extract | run_external_ok | yes | no |
| diff-snapshots | run_external_ok | yes | no |
| repack | run_external_ok | yes | no |
| degrade | run_external_ok | yes | no |

(`verify` subcommands are correctness checks, not benchmarks - they don't
record to either DB.)

### elivagar

Elivagar splits by `BuildKind` (`src/elivagar/commands.rs`), so the answer
varies per command.

| Command | BuildKind | Bench path | sidecar | stderr kv |
|---|---|---|---|---|
| tilegen | MainBinary | run_external_ok | yes | no |
| pmtiles-writer | Example | run_internal / run_captured | no | no |
| node-store | Example | run_internal / run_captured | no | no |
| planetiler | NoBuild | external Java tool | no | no |
| tilemaker | NoBuild | external C++ tool | no | no |

`tilegen` now behaves exactly like a pbfhogg command: brokkr's own best-of-N
external wall-clock `elapsed_ms`, all metrics to sidecar.db as FIFO counters,
stderr captured and discarded (no `elapsed_ms=` contract). It used to be the
one command feeding results.db `kv` via stderr `key=value`; that moved to FIFO
counters on the elivagar side (54f9b07). The example micro-benches record only
brokkr's wall-clock `elapsed_ms`; their stderr and any counters are discarded.
The external baselines build no Rust binary at all.

## Quick decision guide

- Want a metric in `brokkr results`? On the main bench surface, no command
  routes runtime metrics into results.db `kv` anymore - they all use FIFO
  counters -> sidecar.db. The stderr-kv path survives only on the
  read/write/merge/`self` micro-benches (`run_external_with_kv_raw`), where a
  metric needs a `key=value` line **and** a mandatory `elapsed_ms=`/`total_ms=`.
- Want a metric in `brokkr sidecar`? Emit an `@name=value` counter to
  `BROKKR_MARKER_FIFO`. Works for every pbfhogg command and elivagar
  `tilegen` (any path that spawns the sidecar), in bench/hotpath/alloc modes.
- Want it just on your terminal? Print to stdout and run without `--bench`.
