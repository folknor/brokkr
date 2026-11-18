# Output channels: how brokkr captures a run's output

When brokkr runs a project binary under a measurement mode, the binary's
output can travel to the operator's terminal, to the gitignored results DB, to
the gitignored sidecar DB, or nowhere at all. Which of these happens depends
entirely on **which harness path** the command's dispatch chose - the project
binary itself never knows what mode it runs under. This file is the map.

## The two databases

- `.brokkr/results.db` - **gitignored**, kept small. One row per bench
  run:
  `command`, `elapsed_ms`, `commit`, `input`, and a `kv` column of
  arbitrary `key=value` metrics. Queried with `brokkr results`. Two things
  brokkr records itself rather than receiving over a transport channel: the
  per-iteration walls of a `--bench N` run (`run_iterations`, in execution
  order) and the `prev.*` kv pairs naming the previously-recorded run. Neither
  comes from the binary - see `docs/commands/measure.md`.
- `.brokkr/sidecar.db` - **gitignored** (split out of results.db when
  results.db's schema went from v8 to v9). Per-run `/proc` sample
  trajectories, phase markers, application counters, and for a
  harness that runs calls in one process each call's sink ends and boundaries. Queried with `brokkr
  sidecar <uuid>`.

Both live under the project root's `.brokkr/` (never a `--commit` worktree's)
and are resolved identically for every project by `BenchHarness::new_with_lock`
(`src/measure.rs`), which reads git provenance from the build root.
See `docs/commands/measure.md` for the sidecar's sampling/marker protocol.
A script bench's children (`brokkr man bench`) are captured the same way but
stored in the per-user bench store instead, each child's records apart;
`brokkr results` and `brokkr sidecar <uuid>` read them there.

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
(`check`, `test`, measured runs, `crit`; `.brokkr/check-logs/<kind>-<ms>.log`)
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
suppression, and so does the `[sidecar]` line naming a session no results row
names. Non-fatal bookkeeping failures are warnings.

JSON, query tables, outline and other data output retain their exact bytes and
streams. Direct print macros are denied by this crate's Clippy configuration;
function-scoped exceptions identify genuine data and subprocess protocols.
Captured raw replay uses the output renderer without adding a newline. The
shared elapsed-duration formatter affects operator prose, not numeric stored
metrics or table formats. Likewise, a path inside the project prints relative
to the directory brokkr was invoked from in operator lines (and in the run
log, whose header records that directory), while stored values, argv and data
output keep it absolute. Allocation-profile notes have one owner in
`measure::ProfiledMode`.

Captured and passthrough subprocesses run through `src/output/child.rs`, which
owns child polling, typed endings, deadlines, signal forwarding, and bounded
stdout/stderr drains. A captured stream is not replayed while the child
runs; a caller that wants it shown replays the captured bytes afterwards.

## The three transport channels

A running binary can hand data to brokkr three ways:

1. **stdout** - human-readable prose. Either inherited (printed straight to
   the terminal) or captured (drained into a buffer and dropped). Never
   parsed, never stored.
2. **stderr `key=value` lines** - structured metrics, parsed by
   `parse_kv_lines_us` (`src/measure/kv.rs`) on every path that scrapes
   stderr. A line counts only if the text before its first `=`, trimmed,
   is a **field name**: an ASCII letter or `_`, then ASCII letters, digits,
   `_`, `.` or `-` (`rows`, `cold_prepare_us`, `cache.hit-rate`). Any other
   line is prose and is skipped silently, so `Note: auto-selected index
   (sorted=true)` or `time-filter: N versions scanned, M <= cutoff` store
   nothing. The value is the trimmed rest of the line, `=` included
   (`label=a=b` stores `a=b`). The grammar rejects nonconforming names; it
   does not detect prose, so a sentence that opens with a field name and
   `=` (`wheel = zoom, drag = pan`) still reads as a field. On every scraping path, `elapsed_ms` and
   `total_ms` are reserved timing names and never stored as counters, and a
   field in one of brokkr's own namespaces (`meta.`, `env.`, `prev.`,
   `threads.`, `sidecar.`, `provenance.`) is dropped, warned once per key: those
   keys record what brokkr observed, so a child cannot write them. On the
   self-reported paths an `elapsed_ms=` (or its alias `total_ms=`) line is
   **mandatory** - its absence errors the whole run; if both appear, the
   last valid timing line wins. This is the subprocess's **self-reported**
   timing, not brokkr's wall-clock (the `run_external_ok` path uses
   external best-of-N wall-clock instead and ignores the reserved names).
   The value may be **fractional** (`elapsed_ms=6.847`): it is kept as
   exact microseconds in the `elapsed_us` column, with `elapsed_ms` holding
   it rounded to nearest for every existing query and view. Best-of-N and
   `--compare`'s delta use the microsecond reading when both sides have one -
   without it a single-digit-millisecond workload's 100-200us deltas round
   away to nothing. Every other field's value is type-inferred in order:
   `i64` -> finite `f64` -> text (a non-finite float like `NaN`/`inf` falls
   back to text). Those pairs land in the results.db `kv` column and the
   `[result]` line. Rows stored before the field-name rule keep whatever
   keys the older scrape gave them, prose-shaped ones included; a
   `--compare` against such a row lists those as counters present on one
   side only.
3. **FIFO markers + counters** - via `BROKKR_MARKER_FIFO`. The target writes
   `<us> name` (marker) or `<us> @name=value` (counter, value must parse as
   `i64`), `<us>` being microseconds since the process started, optionally
   tagged with a call stream (`<us> %<call_id> ...`, which also carries a
   call sink's sealing and terminal records; see "Marker & counter
   rules" in `docs/commands/measure.md`, The clock included). Drained by
   `SidecarFifo::drain` (`src/sidecar.rs`) into one arrival-ordered capture,
   then into sidecar.db. Any project
   binary can emit them by writing that line format to the FIFO. In pbfhogg
   markers and counters go to the invocation's instrument sink; the CLI
   opens the FIFO as that sink from `BROKKR_MARKER_FIFO` and passes no sink
   when the variable is absent (no sidecar attached).

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

So a metric can reach the `[result]` line three ways: a raw stderr kv pair, a
brokkr-derived throughput field, or (for `write_mbs`) a kv pair that feeds a
derived one. The external wall-clock path also scrapes counters; only its timing comes
from brokkr rather than stderr.

## The harness paths and what each captures

| Harness fn | stdout | stderr kv -> results.db | FIFO -> sidecar.db | Used by |
|---|---|---|---|---|
| `run_passthrough_timed` | inherited (shown) | no | **no** (no FIFO) | run mode (no `--bench`) |
| `run_external_ok` | captured, dropped | **yes** (counters only) | yes | `brokkr mogwai` |
| `run_external_with_kv` | captured, dropped | **yes** | yes | sluggrs `hotpath --bench` |
| `capture_hotpath` (stored via `run_hotpath`) | captured | stderr counters + JSON profile | yes | `--hotpath` / `--alloc`, all projects |

The two external paths differ in exactly one thing: **which clock the recorded
`elapsed_ms` comes from.** `run_external_ok` measures the child from outside;
`run_external_with_kv` believes the target's own `elapsed_ms=` and discards
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
  FIFO emission has nowhere to go because `BROKKR_MARKER_FIFO` is unset.
- In every captured path, the binary's stdout prose is **swallowed** - brokkr
  prints its own `[result]` line instead. To survive bench mode, data must
  go out as a FIFO counter or (on the kv path) an stderr `key=value` line.

## Quick decision guide

- Want a metric in `brokkr results`? Print a non-timing `key=value` line to
  stderr, its key a field name (letter or `_`, then letters, digits, `_`,
  `.`, `-`) with nothing before it. On
  the external wall-clock path (`run_external_ok`: `brokkr mogwai`) it is scraped into `kv` as a counter
  and feeds `--compare`'s `counters:` line; `elapsed_ms` and `total_ms` are
  reserved and ignored there, and the wall stays brokkr's own. On the
  self-reported path (sluggrs `hotpath --bench` through
  `run_external_with_kv`) a mandatory
  `elapsed_ms=`/`total_ms=` line additionally supplies the recorded timing.
- Want a metric in `brokkr sidecar`? Emit an `@name=value` counter to
  `BROKKR_MARKER_FIFO`. Works on any path that spawns the sidecar, in
  bench/hotpath/alloc modes.
- Want it just on your terminal? Print to stdout and run without `--bench`.
