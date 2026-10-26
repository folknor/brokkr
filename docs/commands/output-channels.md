# Output channels: how brokkr captures a run's output

When brokkr runs a project binary under a measurement mode, the binary's
output can travel to the operator's terminal, to the gitignored results DB, to
the gitignored sidecar DB, or nowhere at all. Which of these happens depends
entirely on **which harness path** the command's dispatch chose - the project
binary itself never knows what mode it runs under. This file is the map.

## The two databases

- `.brokkr/results.db` - **gitignored**, kept small. One row per bench
  run: `command`, `elapsed_ms`, `commit`, `input`, and a `kv` column of
  arbitrary `key=value` metrics. Queried with `brokkr results`. Two things
  brokkr records itself rather than receiving over a transport channel: the
  per-iteration walls of a `--bench N` run (`run_iterations`, in execution
  order) and the `prev.*` kv pairs naming the previously-recorded run. Neither
  comes from the binary - see `docs/commands/measure.md`.
- `.brokkr/sidecar.db` - **gitignored** (split out of results.db in schema
  v8->v9). Per-run `/proc` sample trajectories, phase markers, and
  application counters. Queried with `brokkr sidecar <uuid>`.

Both live under the project root's `.brokkr/` (never a `--commit` worktree's)
and are resolved identically for every project by `BenchHarness::new_with_lock`
(`src/measure.rs`), which reads git provenance from the build root.
See `docs/commands/measure.md` for the sidecar's sampling/marker protocol.

The `.brokkr/` directory ignores itself: every command that reads config
creates it with a `.gitignore` of `*` (left alone if one is already there), so
its logs, journals and databases stay out of `git status` and out of a
`git add -A` even in a repo whose own `.gitignore` never mentions it. A project
that commits its `results.db` keeps doing so: git keeps tracking a tracked
file whatever an ignore rule says, and the first add is `git add -f`.

What brokkr itself prints around a measured run (the `run i/n` line, the
single `[sidecar]` summary, the `[run] results.db <id>` line naming the stored
row, the `[run] commit` line for `--commit`) is specified in
`docs/commands/measure.md` ("What a measured run prints").

The prefixed printers in `src/output/console.rs` (`[build]`, `[run]`, `[lock]`, `[strays]`,
`[guard]`, `[bench]`, `[verify]`, `[sidecar]`, `[dry-run]`, ...) all go through
one renderer, which tees each line into the run log when the command keeps one
(`check`, `test`, measured runs, `bench`; `.brokkr/check-logs/<kind>-<ms>.log`)
and keeps a drawn status line intact; none writes to the console directly.
Each prefix is padded to ten columns, and a longer tag (`[ratatoskr]`) overflows
by one space instead of being cut. Lines go to stdout, except `[sidecar]`
narration (stderr, so `brokkr sidecar ... | jq` stays clean) and the deliberately
bounded `[warn]` bookkeeping paths, which attempt both logging and rendering.
These late warnings use stderr so a final JSON summary remains the last stdout
line. Sidecar provenance errors and warnings also stay on stderr. `[lock]` is for lock events,
`[strays]` for the stray-process reaps and `brokkr strays`, and `[guard]` for
`brokkr guard`; `[dry-run]` marks a planned action under `--dry-run`.

Quiet mode hides progress on the console but retains it in the run log.
Warnings, errors, requested test verdicts and lock/guard/stray actions remain
visible. The output channel owns all `[result]` framing; an unretrievable result
(a dirty run or a failed database insert) uses the same printer without quiet
suppression. Non-fatal bookkeeping failures are warnings.

JSON, query tables, outline and other data output retain their exact bytes and
streams. Direct print macros are denied by this crate's Clippy configuration;
function-scoped exceptions identify genuine data and subprocess protocols.
Captured raw replay uses the output renderer without adding a newline. The
shared elapsed-duration formatter affects operator prose, not numeric stored
metrics or table formats. Allocation-profile notes have one owner in
`measure::ProfiledMode`.

Captured and passthrough subprocesses run through `src/output/child.rs`, which
owns child polling, typed endings, deadlines, signal forwarding, and bounded
stdout/stderr drains. Nidhogg ingest progress is replayed live on stderr
(stdout stays captured); update replays both streams live. The replay keeps
pipe bytes unchanged, including chunks split inside a UTF-8 sequence.

## The three transport channels

A running binary can hand data to brokkr three ways:

1. **stdout** - human-readable prose. Either inherited (printed straight to
   the terminal) or captured (drained into a buffer and dropped). Never
   parsed, never stored.
2. **stderr `key=value` lines** - structured metrics, parsed by
   `parse_kv_lines` (`src/measure/kv.rs`) on both external
   paths. The timing contract below applies to the kv harness path only; the
   `run_external_ok` path keeps every pair, `elapsed_ms` included, as an
   ordinary counter beside its own wall-clock. On the kv path a `elapsed_ms=` (or its alias `total_ms=`) line is **mandatory** on that
   path - its absence errors the whole run; if both appear, the last valid timing line wins. The value may be **fractional** (`elapsed_ms=6.847`): it is kept as
   exact microseconds in the `elapsed_us` column, with `elapsed_ms` holding
   it rounded to nearest for every existing query and view. Best-of-N and
   `--compare`'s delta use the microsecond reading when both sides have one -
   without it a single-digit-millisecond workload's 100-200us deltas round
   away to nothing. Every other line's value is type-inferred in order: `i64` -> finite
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

`format_result_line` (`src/measure/record.rs`) adds fields brokkr
computes itself, on top of the raw kv pairs:

- `read_mbs` - appears whenever the input size is known (`config.input_mb`,
  resolved from the dataset config) and the wall is non-zero: `input_mb /
  secs`, with `secs` from `elapsed_us` when the run has it (so a run that
  rounds to 0 ms still gets a figure) and from `elapsed_ms` otherwise.
- `write_mbs` - appears additionally when the run reported an `output_bytes`
  kv pair (so this one *does* depend on the binary emitting it): computed as
  `output_bytes/1e6 / secs`.
- `samples`/`min_ms`/`p50_ms`/`p95_ms`/`max_ms` - only for `run_distribution`
  results, with three decimals from the microsecond summary (`min_ms=0.298`).
  The same values land in results.db's `run_distribution` table, both as
  microseconds (`*_us`) and rounded (`*_ms`).

So a metric can reach the `[result]` line three ways: a raw stderr kv pair, a
brokkr-derived throughput field, or (for `write_mbs`) a kv pair that feeds a
derived one. The external wall-clock path also scrapes counters; only its timing comes
from brokkr rather than stderr.

## The harness paths and what each captures

| Harness fn | stdout | stderr kv -> results.db | FIFO -> sidecar.db | Used by |
|---|---|---|---|---|
| `run_passthrough_timed` | inherited (shown) | no | **no** (no FIFO) | all `--` run mode (no `--bench`) |
| `run_external_ok` | captured, dropped | **yes** (counters only) | yes | every pbfhogg bench command + elivagar `tilegen` + `brokkr mogwai` |
| `measure_external` | captured, dropped | **yes** | yes | elivagar `self` micro-bench; pbfhogg read/write/merge micro-benches (via `run_external_with_kv`) |
| `run_internal` (+ `run_captured`) | captured, dropped | no | **no** (no FIFO) | elivagar example benches |
| `capture_hotpath` (stored via `run_hotpath`) | captured | stderr counters + JSON profile | yes | `--hotpath` / `--alloc`, all projects |

The two external paths differ in exactly one thing: **which clock the recorded
`elapsed_ms` comes from.** `run_external_ok` measures the child from outside;
`measure_external` believes the target's own `elapsed_ms=` and discards
the external wall. Both record microseconds in `elapsed_us` - the external
path from its exact `Duration` - and `elapsed_ms` is that value rounded to
nearest.

`Measurement` retains stdout and stderr for every captured iteration. Callers
may inspect either stream before recording; storage uses the winner's metrics.
Missing or invalid hotpath JSON fails the iteration and preserves its sidecar
under `dirty`, rather than recording a profile mode without a profile.

Both scrape non-timing stderr `key=value` pairs for counters, because stderr is
captured either way and the scrape is free. A wall on its own cannot
distinguish "the code got faster" from "the code did less"; the counters are
what answer that, and they drive `--compare`'s `counters:` line. The reserved
`elapsed_ms` and `total_ms` keys are omitted from the external path's counters;
on the self-reported path they supply the recorded wall.

There was briefly a third path combining an external wall with counter scraping.
It differed from `run_external_ok` by a bool that the shared body already took,
so it is gone: scraping is now unconditional.

Two consequences worth internalizing:

- **Run mode (no `--bench`) stores nothing and attaches no sidecar.** stdio
  is inherited, so the binary's own prose reaches the terminal verbatim, and
  `emit_counter` no-ops because `BROKKR_MARKER_FIFO` is unset.
- In every captured path, the binary's stdout prose is **swallowed** - brokkr
  prints its own `[result]` line instead. To survive bench mode, data must
  go out as a FIFO counter or (on the kv path) an stderr `key=value` line.

## Per-command table

`sidecar` = `emit_counter`/`emit_marker` reach sidecar.db (`brokkr sidecar`).
`kv` = stderr `key=value` reaches results.db (`brokkr results`): `counters`
means scraped into `kv` alongside brokkr's own wall-clock, with no
`elapsed_ms=` contract. Both columns describe **bench mode** (`--bench N`);
run mode stores nothing for any command.

### pbfhogg

All measurable pbfhogg commands share one dispatch path
(`run_pbfhogg_wallclock` -> `run_external_ok`), so the answer is uniform:
sidecar **yes**, stderr-kv **no** in the sense of the `elapsed_ms=` timing
contract - any stderr `key=value` lines are still scraped as counters (see
above). `elapsed_ms` in the result is brokkr's own best-of-N wall-clock, not
self-reported.

| Command | Bench path | sidecar | stderr kv |
|---|---|---|---|
| inspect | run_external_ok | yes | counters |
| check-refs | run_external_ok | yes | counters |
| check-ids | run_external_ok | yes | counters |
| sort | run_external_ok | yes | counters |
| cat | run_external_ok | yes | counters |
| tags-filter | run_external_ok | yes | counters |
| getid | run_external_ok | yes | counters |
| getparents | run_external_ok | yes | counters |
| renumber | run_external_ok | yes | counters |
| merge-changes | run_external_ok | yes | counters |
| apply-changes | run_external_ok | yes | counters |
| add-locations-to-ways | run_external_ok | yes | counters |
| time-filter | run_external_ok | yes | counters |
| diff | run_external_ok | yes | counters |
| build-geocode-index | run_external_ok | yes | counters |
| extract | run_external_ok | yes | counters |
| multi-extract | run_external_ok | yes | counters |
| diff-snapshots | run_external_ok | yes | counters |
| repack | run_external_ok | yes | counters |
| degrade | run_external_ok | yes | counters |

(`verify` subcommands are correctness checks, not benchmarks - they don't
record to either DB.)

### elivagar

Elivagar splits by `BuildKind` (`src/elivagar/commands.rs`), so the answer
varies per command.

| Command | BuildKind | Bench path | sidecar | stderr kv |
|---|---|---|---|---|
| tilegen | MainBinary | run_external_ok | yes | counters |
| pmtiles-writer | Example | run_internal / run_captured | no | no |
| node-store | Example | run_internal / run_captured | no | no |
| planetiler | NoBuild | external Java tool | no | no |
| tilemaker | NoBuild | external C++ tool | no | no |

`tilegen` now behaves exactly like a pbfhogg command: brokkr's own best-of-N
external wall-clock `elapsed_ms`, its metrics to sidecar.db as FIFO counters,
and non-timing stderr `key=value` lines scraped into results.db `kv` as
counters. An stderr `elapsed_ms` or `total_ms` does not replace the wall or
appear as a counter. Its runtime metrics moved from stderr `key=value`
to FIFO counters on the elivagar side (54f9b07). The example micro-benches record only
brokkr's wall-clock `elapsed_ms`; they don't scrape stderr into result KV, but
replay their captured stdout and stderr to the terminal.
The external baselines build no Rust binary at all.

### nidhogg

The `tiles` lifecycle benchmark measures the server's start-to-shutdown wall
with brokkr. After the readiness message, it parses the remaining stderr
`key=value` lines into result KV pairs; `elapsed_ms`/`total_ms` are reserved
timing names and are not stored as counters. This path uses no sidecar FIFO.

| Command | Bench path | sidecar | stderr kv |
|---|---|---|---|
| tiles | `run_internal` + server lifecycle | no | shutdown counters |

## Quick decision guide

- Want a metric in `brokkr results`? Print a non-timing `key=value` line to stderr. On
  the external wall-clock path (`run_external_ok`: every pbfhogg command,
  elivagar `tilegen`, `brokkr mogwai`) it is scraped into `kv` as a counter
  and feeds `--compare`'s `counters:` line; `elapsed_ms` and `total_ms` are
  reserved and ignored there, and the wall stays brokkr's own. On the
  read/write/merge/`self` micro-benches (`measure_external`) a
  mandatory `elapsed_ms=`/`total_ms=` line additionally supplies the recorded
  timing.
- Want a metric in `brokkr sidecar`? Emit an `@name=value` counter to
  `BROKKR_MARKER_FIFO`. Works for every pbfhogg command and elivagar
  `tilegen` (any path that spawns the sidecar), in bench/hotpath/alloc modes.
- Want it just on your terminal? Print to stdout and run without `--bench`.
