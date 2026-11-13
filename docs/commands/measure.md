# Measurement modes: --bench, --hotpath, --alloc, --stop

Every measurable command is a top-level brokkr subcommand. Measurement mode is
a flag:

```
brokkr <command> [--bench [N] | --hotpath [N] | --alloc [N]] [command options]
```

- No flag - build, run once, print timing. Acquires lockfile, no DB storage.
- `--bench` - full benchmark: lockfile, 3 runs (or N), best-of-N stored in DB.
  All N per-iteration walls are stored too (`run_iterations`), in execution
  order - see "Per-iteration walls" below.
- `--hotpath` - function-level timing via hotpath feature. 1 run (or N).
- `--alloc` - per-function allocation tracking via hotpath-alloc feature. 1
  run (or N).
- `--stop <marker>` - kill the child when this FIFO marker is emitted. Allows
  benchmarking a specific phase in isolation. The SIGKILL exit is treated as
  success.

Sidecar-enabled subprocess modes sample `/proc` metrics at 100ms and provide
`BROKKR_MARKER_FIFO` for phase markers. Closure-only modes such as
`run_internal` and `run_distribution` do not use a sidecar. Collected sidecar
data is stored in `.brokkr/sidecar.db` (gitignored), including partial data
when a measured child is OOM-killed.

Dataset paths resolve from `brokkr.toml` automatically. All flags go after
the command name.

## Measurable commands per project

The measurable commands are pbfhogg's data ops, elivagar's tilegen/etc., and
nidhogg's query/ingest. Piners' `corpus` is **not** measurable: it takes no
measurement flag, and its timing is the parity run row's `wall_ms` (why:
`docs/commands/corpus.md`). Pbfhogg's `library SCENARIO` takes `--bench`, and `--hotpath`/`--alloc` for a
scenario its harness declares hotpath, building the
`[pbfhogg.library.harness]` member with the instrumentation feature added;
every process is a full verification (`brokkr man library`).

## Command plans

Pbfhogg, elivagar and nidhogg use `src/measure/runner.rs`. Binary plans run a
built executable; external plans materialize tools outside the measured samples;
HTTP/server lifecycle plans provide their own protocol operations. All use one
`MeasurementSession` for the lock, paths, optional build and request provenance.
A binary plan's argv is resolved against the built executable before the
first sample; that resolved invocation is what runs and what `cli_args`
records. Pbfhogg adds `--overwrite` there when the executable accepts it (see
`docs/projects/pbfhogg.md`).

Dry-run validates inputs and argv without acquiring a measurement session,
building, downloading tools, starting servers, or creating run artifacts.
With nothing built, it prints the argv as planned, before any flag resolved
against the executable. The planned argv is the one that runs: a lifecycle
plan spawns its child with it (nidhogg's tiles server, elivagar's examples
with the internal run count the mode gives them), and a plan with no single
argv says what it will do instead (nidhogg's API bench prints its endpoint and
selected queries; see `docs/projects/nidhogg.md`).
It still follows the command's lock policy, so a measured command can wait for
the brokkr lock before it reports the plan.
Input hash verification may refresh `.brokkr/hash_cache`; this validation cache
is the sole permitted planning write. Elivagar's argv construction creates no
directories. API dry-run derives queries without contacting the running
server, and refuses a `--query` that selects none.

Run executes once without persisting result or sidecar records. Stored modes
use the request's measurement mode and brokkr invocation; per-row fallback
fields do not exist. Row command names and subprocess `cli_args` retain their
previous spelling, except that pbfhogg rows gain `--overwrite` from builds that
accept it (examples and HTTP/server lifecycles retain absent `cli_args`).

Declared sample scratch is reset before each sample and removed after success.
A failed process, capture, or artifact promotion retains its current output.
Shared input caches, downloaded tools and durable output archives are distinct
from sample scratch. Tilegen's checkpoint directory remains available for
`--skip-to`; elivagar owns its checkpoint reset contract.

## Benchmark harness

`BenchHarness` (in `src/measure.rs`) provides:
- Exclusive lockfile (prevents parallel bench/verify/hotpath runs). The
  lock is re-entrant *within* one brokkr process: a command that
  internally invokes another locked command shares the same hold, and
  the flock releases when the last guard drops. Ratatoskr's `sync
  --gate all` relies on this to hold the lock across its whole sweep
  (each swept bench still acquires internally); `sync --all` holds it
  sweep-wide too, with a single acquire. Cross-process exclusion is
  unchanged.
- SQLite result storage with git commit, hostname, env snapshot
- `measure(config, program, source, sidecar, closure)` collects a `Measurement`
  through one `MeasurementLoop`. Its `WallSource` is fixed for the whole run:
  external wall, self-reported stderr timing, closure timing, or a required
  marker span. Missing span markers fail the measurement; spans and whole
  process walls are never ranked together. Iterations retain streams, typed
  endings, optional sidecar data and observer failures. Ties keep the first run.
- `record(config, measurement, post)` postprocesses the winner and records the
  results row before its sidecar. Measurement, postprocessing and row-insert
  failures preserve captured evidence under `dirty`; storage failures are
  reported. There is no separate sidecar commit for callers to arrange.
- `run_internal(config, closure)` uses closure timing and best-of-N, without
  a sidecar. `run_hotpath(config, program, source, closure)` uses the same
  loop with a FIFO and the caller's wall source, so the profile is the one
  attached to the iteration that wall ranks best; `capture_hotpath` receives
  that FIFO and uses the session's held lock and stop marker. Missing or
  invalid profile JSON fails the iteration while retaining its captured
  evidence.
- A self-reported wall replaces only the iteration's elapsed fields with the
  `elapsed_ms=` capture parsed from stderr; its counters, an answer's
  `meta.exit_code`, an observer's additions and a hotpath profile stay. A
  child that printed no timing fails the sample.
- `runner::execute_cell(mode, harness, cwd, cell, hooks)`
  (`src/measure/runner.rs`) is the one place a measured invocation's mode is
  decided: run with output passed through, benchmark, or profile. A cell is
  the invocation, its row subject, its accepted exits and its wall source;
  the planned runner's plans are cells with the external wall, and pbfhogg's
  `read`, `write` and `merge` cells (`src/pbfhogg/bench_cells.rs`) are cells
  with the self-reported wall in every mode. No command accepts `--hotpath`
  or `--alloc` and runs a plain child, and every measured build takes its
  features from `MeasureRequest::build_features`, which adds the mode's
  instrumentation feature.
- `run_external(config, binary, args)` - subprocess timing
- `run_distribution(config, closure)` - distribution timing
  (min/p50/p95/max). The closure returns each sample as a `Duration`; the
  summary is computed in microseconds and stored in both `run_distribution`'s
  `*_us` columns and, rounded, its `*_ms` columns (see below).

The mode a row records (`bench`/`hotpath`/`alloc`) comes from the harness,
set once from the request; the same value prints as `mode=` on the `[result]`
line and lands in the sidecar provenance, so `brokkr sidecar` shows it.

Where brokkr times the child itself (`run_external`, `capture_hotpath`) it
keeps the exact wall as `elapsed_us` and picks best-of-N on it; `elapsed_ms`
is that value rounded to the nearest millisecond, never floored - flooring
reads every run as up to a millisecond faster than it was. `run_distribution`
does the same with its samples: each is kept in microseconds, every percentile
is taken over the microseconds, and the row's `elapsed_us` is the fastest
sample. A sample under half a millisecond (a nidhogg API query) used to record
0 ms at every percentile; the `*_ms` columns still read 0 for it, which is why
they are not the only record. `run_internal` records whatever `elapsed_us` its
closure returned, and integer milliseconds only when the closure had nothing
finer.

The `[result]` line prints `elapsed_ms=` and a distribution's `min_ms=`/
`p50_ms=`/`p95_ms=`/`max_ms=` with three decimals whenever the microsecond
reading exists (`min_ms=0.298`), and as integers otherwise.

A stored mode refuses a dirty measured tree unless `--force` is given, and
then warns that results will not be stored. The tree is the build root, so a
`--commit` worktree is always clean and stores whether or not `--force` was
passed - `--force` permits a dirty tree, it never decides storage. On a clean
tree it warns that it has no effect (every measured command, ratatoskr's
`sync --bench` included), so an A/B whose baseline is `--commit` and
whose other arm is a dirty working tree shows which arm filed a row.

To A/B an uncommitted variant and keep every cell, commit it locally first
and drop the commit afterwards: with the baseline as `--commit <parent>` and
the variant as the now clean tree (or both arms as `--commit`), every cell
stores a row carrying its argv, captured env and provenance, with its sidecar
profile under the row's id, `brokkr results` lists them, and `brokkr results
--compare` pairs the arms. The rows keep the hash after the commit is gone. A
dirty-tree cell that `--force` permits keeps its sidecar profile but has no
results row: its id is named only on that run's `[sidecar]` line (below), and
the `dirty` alias reaches only the latest such session.

If the `results.db` insert fails, the `[result]` line still prints
(unconditionally, since the row cannot be looked up later) and the sidecar
trajectory is stored under `dirty` before the error is returned - a DB failure
costs the row, not the measurement.

Results in `.brokkr/results.db` per project (gitignored).

## What a measured run prints

One fact, one place. A run with a clean tree prints, in order:

- With `--alloc`, `[hotpath] NOTE: alloc profiling - wall-clock times are not
  meaningful` before the iterations, even without `--verbose`.
- `[bench] run i/n` before each iteration, **only when n > 1** (a single run
  has nothing to count). Quiet-gated.
- the `[result]` line, the one place the wall time appears (quiet-gated, except
  when the row could not be stored). A `--hotpath`/`--alloc` run whose row
  could not be stored (a dirty tree, a failed insert) follows it with the
  per-function report `brokkr results <uuid>` would have shown (default
  `--top`), since there is no uuid to ask with. A stored run does not print
  the report.
- `[run] results.db a1b2c3d4`, the stored row's short id - the one line
  naming it, and not hidden by quiet mode, since it is how the run is found
  again (`brokkr results a1b2c3d4`, `brokkr sidecar a1b2c3d4`).
- one `[sidecar]` summary line on stderr once the profile is stored: the best
  run's sample, marker and counter counts, with `best run k/n:` when there
  were several (`(sidecar.db)`). The profile shares the results row's id, so
  the line does not repeat it, and it is quiet-gated. A trajectory with no
  results row is filed under the `dirty` alias, and only then does the line
  name its own id (`sidecar.db a1b2c3d4, filed as dirty`); that line is then
  the one place the id appears, so like the `results.db` line it is not hidden
  by quiet mode (measured runs are quiet unless `--verbose`). Moving the alias
  removes nothing: every earlier such session keeps its rows under its own id
  until `brokkr invalidate`. If the alias cannot be written, a warning says so,
  the line reads `sidecar.db a1b2c3d4, no results row`, and the run's outcome
  is what it would have been: the alias is bookkeeping, not part of the
  measurement. A harness measurement writes its session in one transaction
  after the last iteration, so a storage failure leaves no session behind
  under an id no line named. A library measurement is the exception: it
  stores each process as soon as it is judged, keeping the earlier ones if a
  later write fails, and names its session on that error (`brokkr man
  library`, Measuring a scenario).

Sent to the run-log channel (`detail`) instead of the console: the per-run
sidecar attach (`sidecar attached to pid P, run k`, one-indexed like `run i/n`)
and per-run counts with the child's wall time, and the `--hotpath`/`--alloc`
header naming the target and instrument (the command line already says
both). Every measured run (and `sync --bench`) opens one for this:
`.brokkr/check-logs/measure-<ms>.log` under the config dir, the newest ten
kept, apart from `check`'s and `test`'s logs (see `docs/commands/check.md`,
the run log).

With `--commit REF`, the worktree's commit is named once
(`[run] commit abc1234 (subject)`, quiet-gated), by the shared worktree
lifecycle in `context::with_worktree`, whichever command asked for it. Whether
the worktree was reused or cut, and the isolated target dir the build is
pinned to, use the same run-log channel; under a command that keeps no run
log (`run`, `verify` or `sync` with `--commit`) they print as `[run]` lines
instead, quiet-gated, rather than vanish. A stale-replacement or an eviction is
printed.

A non-fatal problem is a `[warn]` (never hidden by quiet mode), not an
`[error]` and not a `[sidecar]`/`[lock]` narration line: a failed sidecar
backup, a stale rustc guard, a lock-metadata write that degraded, a history
write that failed (history and lock metadata warnings use stderr without
the console render lock, so late bookkeeping leaves stdout trailers intact),
a snapshot file that could not be removed after a download.
An unrecognised `cargo_profile` in the results DB warns once per distinct
value, not once per row.

## Per-iteration walls

`--bench N` reports best-of-N, but stores every iteration's wall in the
`run_iterations` table (`run_id`, `run_idx`, `elapsed_ms`, `elapsed_us`) and
renders them on `brokkr results <uuid>`:

```
elapsed  592200.412 ms (best of 3: 592200.412 / 611400.090 / 634100.733 ms)
```

`elapsed_us` is NULL unless the loop measured every iteration in microseconds
(`run_external_ok`, `run_distribution`, `measure_external` - from the
target's self-reported `elapsed_ms=`, the same clock its row records - and
`run_internal`/`run_hotpath` when each iteration's result carried an
`elapsed_us`). `Measurement` retains them until `record` writes the row and sidecar together. A list with a gap is not
stored at all rather than stored in part, since a partial list cannot stay
aligned with the execution order. Rows without it render the integer
milliseconds.

The walls are listed **in execution order and never sorted**, because the order
is the signal:

- iteration 1 fastest, 3 slowest -> drift *under* the run (e.g. SLC-cache
  exhaustion on a scratch-heavy cell)
- iteration 1 slowest, 2/3 faster -> cold page cache on the first pass

Those are opposite diagnoses. This is why the walls do **not** live in
`run_distribution` (min/p50/p95/max): sorting the samples discards the ordering,
and a p50 cannot tell the two apart.

Collected by every harness loop (`run_internal`, `run_hotpath`,
`run_external_ok`, `measure_external`, `run_distribution`) and attached
to the best-of-N `BenchResult` before recording. One-iteration measurements retain that one wall too; rows recorded before
schema v16 have no iteration data - those walls
were discarded at measure time and cannot be reconstructed from `elapsed_ms`.
Rows recorded before schema v19 have no per-iteration microseconds, for the
same reason.

## What ran before

Every stored row also records its predecessor as `prev.*` kv pairs:
`prev.command`, `prev.uuid`, `prev.input_file`, `prev.gap_seconds`, read from
the most recent row in the same project's results DB. Dirty runs and runs
recorded by other projects are outside that store.

`prev.gap_seconds` is measured from the previous run's timestamp to **the moment
this run started**, not to the moment it was recorded. Rows are timestamped when
inserted (i.e. when that run finished), and the row is written after the last
iteration - so reading the clock at record time would count this run's own
duration as idle time, and a 10-minute cell launched straight after its
predecessor would report a ~10-minute gap and look cache-cold. The harness
stamps the start of its measurement loop for exactly this reason.

This is the cheapest available proxy for page-cache state. A cell that reads the
same file its predecessor just read starts warm and reports a wall the code
didn't earn; `prev.gap_seconds` + `prev.input_file` together usually distinguish
"this baseline was warm" from "this baseline was fast". If there is no prior
row, no `prev.*` fields are added. A database lookup error is surfaced instead
of recording a row without its predecessor provenance.

## Sidecar profiler

Sidecar-enabled subprocess measurements sample `/proc/{pid}/stat`,
`/proc/{pid}/io`, and `/proc/{pid}/status` at 100ms intervals and read phase
markers from a FIFO. Their data is buffered in memory during the run and
bulk-inserted to `.brokkr/sidecar.db` (gitignored) after the child exits.
Closure-only measurements such as `run_internal` and `run_distribution` do not
attach a sidecar.
Results DB (`.brokkr/results.db`) stays small and gitignored.

Key files: `src/sidecar.rs` (sampler and FIFO),
`src/measure/execution.rs` (iteration collection),
`src/measure/capture.rs` (child capture),
`src/measure/record.rs` (row and sidecar persistence),
`src/sidecar_backup.rs` (backup rotation), and
`src/db/sidecar.rs` (`SidecarDb`).

The child process receives `BROKKR_MARKER_FIFO` env var pointing to a named
pipe, `.brokkr/.sidecar-<token>.fifo` with a fresh 128-bit random token per
measurement or library run (`src/sidecar/namespace.rs`; what `brokkr clean`
removes is in `brokkr man clean`). Only a child brokkr attaches a FIFO to
gets the variable: brokkr drops `BROKKR_MARKER_FIFO` and
`BROKKR_HARNESS_ARTEFACT_DIR` from its own environment when a command starts
(not when it runs as the test harness shim, which forwards its environment
unchanged to the program cargo meant to run), so a
brokkr started under another brokkr's sidecar passes that FIFO to none of its
children, measured or not (configured environment can still set the name
explicitly). The shared supervisor in `src/output/child.rs` waits on the child (a pidfd between ticks), handles
stop markers, deadlines and interrupts, and drains stdout/stderr with a bounded
settle period. It records one typed ending (`Exited`, `Signalled`, `StopMarker`,
`Deadline`, `Interrupted`, or `WaitFailed`) before waiting for the drains. `src/sidecar.rs`
supplies only the per-tick `/proc` sampler and FIFO observer. Sidecar data is
stored even when the child fails (OOM, signal, deadline, or non-zero exit).

## Querying sidecar data

`brokkr sidecar <uuid>` - bare form is the per-phase JSONL summary (pass
`--human` for a table). View selectors are mutually exclusive:
- `--samples` - raw JSONL /proc samples
- `--markers` - raw JSONL marker events
- `--durations` - START/END pair timings. In `--human`, repeated span names
  collapse to one aggregate row each (JSONL stays one object per span).
- `--counters` - application counters; `--grep <SUBSTR>` filters by name
- `--stalls` - `*_wait_ns` rollup (see below)
- `--call <CALL_ID>` - not a view: restricts any view but `--compare` to one
  call stream
- `--stat <field>` - min/max/avg/p50/p95
- `--compare <a> <b>` - two runs' phases side by side, a repeated name paired
  by ordinal (see "How `brokkr sidecar` renders")

Filter flags `--phase`, `--range`, `--where` compose with `--samples` and
`--stat`; `--fields`/`--every`/`--head`/`--tail` only with `--samples`. A UUID
is required except for `--compare`; the `dirty` pseudo-UUID resolves to the
most recent failed/dirty-tree run (see "Run lifecycle" below for how it is
set), and each earlier one by the id its `[sidecar]` line named. `--bench N` stores **all N runs** in sidecar.db but marks
`best_run_idx` (the run whose wall-clock matched the reported `elapsed_ms`);
the default view and `--stat`/`--phase` read that best run unless another
`run_idx` is selected.

The UUID argument is a prefix, matched literally (`_`/`%` are not wildcards),
and it must name **one** session: a prefix shared by several sessions is an
error listing the candidates rather than a merged timeline, and an empty
prefix is refused. It resolves across `results.db`, `sidecar.db` and the
current project's script-bench runs at once (below), so a prefix naming a
typed run and a bench run is ambiguous too, the bench candidates marked
`(script bench)`; a typed run counts whether or not it has sidecar data. A
prefix naming only another project's bench run says which project it
belongs to. Reading the bench store means a typed query fails when that
store cannot be read, or the configured results server cannot be reached,
as `brokkr results` does.

## Script-bench children

A script-bench run (`brokkr man bench`) stores, for every child of every
iteration, what the sidecar captured: samples, markers, counters, sink ends,
capture losses by cause and the clock check (`brokkr man results`, Script
benches). `brokkr sidecar <uuid>` reads one child of each selected
iteration through every view above, the child's records under its own
clock as they were written, nothing rebased.

- `--run N` selects the iteration (`all` for every one); by default the
  result's iteration, or for a run without a result the last one stored.
- `--child N` selects the child within each selected iteration, in launch
  order; by default the iteration's only child. The whole selection is
  judged before anything prints: an iteration that launched several
  children without `--child`, none, or fewer than `--child` names refuses
  the query, naming every such iteration. One selected iteration that
  launched no child, with no `--child`, prints its outcome and `launched no
  child` instead. `--child` on a typed run is refused.
- JSONL `run` is the iteration, in sample records too (a typed sample
  carries none). The provenance header on stderr names the bench, project,
  start, commit and outcome, then the iteration, the child's argv, PID,
  start offset in the block, wall and any abnormal ending. Each selected
  child's capture loss is warned with its exact causes, and its clock check
  as for a typed run, labelled `--run N --child N`.
- A child's interval is known, from its spawn to its stored wall, so its
  process timeline closes at the wall, not at its last sample: the last
  phase ends there (or at a marker stamped past it, which moves only the
  segment end), a markerless child is one `(all)` segment from 0 to the
  wall, and the default summary and `--compare` need no sample. A phase no
  sample landed in is still a phase: the `--human` table shows it with `-`
  readings, JSONL with `samples: 0`, and its readings `null` on a
  `--compare` side, never zero. Phase records and `--compare` sides add
  `end_us` and `duration_us`; the summary record's `wall_ms` and `wall_us`
  are the child's wall, with `samples` and `peak_threads` (`null` without a
  sample). `--stalls` reads against the child's wall.
- A child's call streams have no stored boundaries, so the default summary
  adds a `call_sink` record (`run`, `call`, the terminal record that arrived
  or `null`, the sealing count) for a call whose sink ends arrived, which a
  typed run without boundaries does not print. `--call` knows a call by its
  records or the child's census of call ids; one only the census names is
  said to have records the capture did not keep, and an unknown one under
  an overflowed census is said to be possibly unrecorded.
- `--compare` takes `UUID`, `UUID/ITER` or `UUID/ITER/CHILD` per side,
  each side on its own: a bench side is its result's iteration and that
  iteration's only child unless the operand names others, and a run without
  a result never compares, as `brokkr results --compare` holds. A typed side
  takes no qualifier and is its best run; typed and bench sides mix, both
  being one process stream. `compare_summary` gains `a_iteration`/`a_child`
  (and `b_`) for a bench side, and the table heads it `uuid/ITER/CHILD`.

Every human table prints whole milliseconds, so a sub-millisecond phase of a
short child reads `0ms` there; the JSONL carries microseconds.

## Sample fields

Each 100ms sample is assembled by `read_proc_metrics` from three files, and
**all three reads must succeed or the whole sample is dropped** (a partial
read at process exit would corrupt phase deltas). Field names below are the
tokens accepted by `--fields`, `--where`, and `--stat`.

| Field | Source | Meaning |
|---|---|---|
| `i` | - | sample index |
| `t` | - | seconds since brokkr spawned the process, from an `Instant` taken just before the spawn (always present in `--samples` JSON; Marker & counter rules, The clock) |
| `rss` | status `VmRSS` | resident set (kB) |
| `anon` | status `RssAnon` | anonymous RSS (kB) - the usual "real memory" signal |
| `file` | status `RssFile` | file-backed RSS (kB) |
| `shmem` | status `RssShmem` | shared-memory RSS (kB) |
| `swap` | status `VmSwap` | swapped-out (kB) |
| `hwm` | status `VmHWM` | peak RSS high-water mark (kB), carried monotonically |
| `vsize` | stat field 22 | virtual size (kB) |
| `utime`/`stime` | stat 14/15 | user/kernel CPU jiffies (decode via `_SC_CLK_TCK`) |
| `threads` | stat field 20 | live thread count |
| `minflt`/`majflt` | stat 10/12 | minor/major page faults (cumulative) |
| `rchar`/`wchar` | io | bytes read/written via syscalls (cumulative) |
| `rd`/`wr` | io `read_bytes`/`write_bytes` | bytes to/from the block layer |
| `cwr` | io `cancelled_write_bytes` | writes cancelled before flush |
| `syscr`/`syscw` | io | read/write syscall counts |
| `vcs`/`nvcs` | status | voluntary / non-voluntary context switches |

## Sample filters & projection

Compose these with `--samples` (and, where noted, `--stat`):

- `--fields rss,anon` - project to a subset (JSON always keeps `t`).
- `--where "majflt>0"` - filter rows; operators `>`, `<`, `>=`, `<=`, `==`,
  `!=` against any field above.
- `--every N` - keep every Nth row (downsample).
- `--tail N` then `--head M` - tail is applied first, so `--tail 100 --head 10`
  is "last 100 rows, then the first 10 of those".
- `--range 10.0..82.0` - seconds window; `--phase FOO` - restrict to a marker
  phase (name-resolution rules above).
- `--stat <field>` prints min/max/avg + p50/p95 (linear-interpolation
  percentiles, matching `sidecar::linear_percentile`).

A `--where` that does not parse, or a field name outside the table above in
`--where`/`--fields`/`--stat`, is an error before anything prints - never a
silently skipped filter or dropped column.

## Marker & counter rules

The FIFO carries these line types (parsed in `SidecarFifo::drain`,
`src/sidecar.rs`):

- **Marker** - `<ts_us> <name>`. Assigned a monotonic `marker_idx` in arrival
  order; the last name seen is also recorded, with the time it was seen, on
  the live hold's in-memory observation table, so `brokkr lock` shows it as
  `last marker: NAME (12s ago)` over the holder's control socket. It is
  cleared whenever a FIFO is created or dropped, so an earlier run's marker in
  the same hold never stands in for the current run's. Markers are
  point-in-time bookmarks - the protocol
  itself knows nothing about spans or pairs. The name is untrusted text; every
  JSONL view encodes it with serde_json, so control characters are escaped.
- **Counter** - `<ts_us> @<name>=<value>`. The value **must parse as `i64`**
  or the line is rejected. `<ts_us>` that doesn't parse is rejected for
  either type. What a timestamp means is under "The clock" below.
- **Call stream** - `<ts_us> %<call_id> <name>` and
  `<ts_us> %<call_id> @<name>=<value>` are the same records in one call's
  stream, for a library harness that runs several calls in one process.
  `call_id` is 1 to 64 bytes of `A-Z a-z 0-9 _ . -`, so nothing is ever
  escaped; it names one invocation within one process run, the producer
  keeps it unique, and a human label belongs in the harness artefact, not
  the stream. Lines without a tag are the process-wide stream, as before. A
  payload starting with `%` is only ever a tagged record: a bad call id, a
  missing name or a bad counter is rejected, never read as an untagged
  marker, so an untagged marker name cannot start with `%`. The producer
  writes each line, newline included, in one write of at most 512 bytes; the
  reader's own line limit (`MeasurementTunables`) is a defence, not that
  contract. A tag says which invocation emitted a line, not which resources
  it measures: a whole-process reading (mallinfo, page faults) stays
  whole-process when a call's sink emits it.
- **Sink end** - in a tagged record only, a body starting `@@` is a sealing
  emission, `<ts_us> %<call_id> @@<name>=<value>` (name 1 to 400 bytes
  without `=`, value an `i64`), and a body starting `!` is a terminal
  record, exactly `<ts_us> %<call_id> !sealed drops=N` or `... !failed
  drops=N` (N a `u64` in decimal, no sign, no leading zero). Any other
  tagged body starting `@@` or `!` is rejected, so a tagged counter name
  cannot start with `@` and a tagged marker name cannot start with `!`.
  Untagged lines read as before (`<ts> @@x=1` is a counter named `@x`). A
  library harness's call sink writes them when it is sealed; what they mean
  is `brokkr man library`, The call stream. They are never markers or
  counters in any view. sidecar.db stores them (`sidecar_sink_records`,
  since its v10, a terminal's drops as exact decimal text) and the default
  summary reads them; a run stored before v10 had no place for them and
  was stored as incomplete, which it stays.

**The clock.** Every `<ts_us>` on the FIFO is a non-negative integer count
of microseconds on one monotonic elapsed-time clock whose zero is the start
of the process brokkr spawned. Every thread, call stream, counter and sink
end shares that origin, and so do a library harness's call boundaries
(`brokkr man library`). Nothing resets it: not opening the FIFO, not
initialising instrumentation, not beginning a call, not the first record.
UNIX-epoch stamps do not conform. The origin matters because brokkr stamps
each `/proc` sample (the `t` field) from an `Instant` it takes just before
spawning that process, and every view that lines markers up against samples
(the per-phase summary, `--phase` with `--samples`/`--stat`/`--counters`)
compares the two clocks directly. The alignment is approximate: it includes
the launch overhead between brokkr's `Instant` and the process start, and
however the writer estimates its own start (pbfhogg reads its process age
from procfs; an `Instant` taken as early as possible in `main` is the
portable approximation). A writer that takes its origin later, at its first
FIFO open or first record, conforms only as far as that delay is negligible:
every stamp is early against the samples by it. What only subtracts two
stamps of one stream (`--durations`, a marker-span wall) does not depend on
the origin, and `--stop` and `brokkr lock`'s last marker read names and
brokkr's own observation time, not stamps.

brokkr checks what it can. At the end of a capture, any retained record stamped
negative or later than the run's wall plus one second is counted as outside
the run interval, warned about once, and stored on the run's sidecar.db
summary (`clock_outliers` and the smallest such stamp, since v11); every
`brokkr sidecar` query of that run repeats the warning, one line per run.
The records themselves are stored and shown as written; nothing is rebased.
Such a record is off the contract clock, or was written after the process
exited by a descendant still holding the FIFO. Passing proves nothing: a
late origin stamps early, never late, so it passes. A run stored before v11
was never checked, and the run's wall is no longer in sidecar.db; the
upgrade marks one only for a stamp negative or at or past 10^15 us and
leaves the rest unchecked. That threshold is 31.7 years, an operational
assumption that no measured process in the stored history ran that long,
not a protocol rule; every UNIX-epoch stamp after 2001-09-09 exceeds it. No
reader interprets or rebases a stamp by its magnitude. A ratatoskr harness revision that stamps
UNIX-epoch microseconds produces exactly such runs, at capture and in the
stored history alike.

The capture keeps every record in the one order it arrived in, markers,
counters and sink ends alike, and sidecar.db stores each record's position
in it (`record_idx`, since v10; NULL on older rows, whose cross-kind order
was never recorded): counters and sink ends read back in arrival order,
markers in `marker_idx` order, the same order among markers. A library
measurement also stores each call's boundaries and its sink's own account
from its call document (`sidecar_calls`; `brokkr man library`, Measuring a
scenario). Every rejected line, including invalid UTF-8
and a record cut off without a newline when the writers close, warns and
marks the run's stored trajectory incomplete, so a later `brokkr sidecar`
query still says so. A query that finds no fault does not mean every line
arrived: the FIFO is best effort, and a library harness's own end-of-call
snapshot is the authoritative figure. brokkr asks the kernel for a 1 MiB
FIFO buffer (the unprivileged default ceiling) so a producer between two
drains drops fewer lines; where that is refused the default stays.

Every view groups markers and counters by `(run, call)` stream. `--call
CALL_ID` restricts a view to one call; a call id that the selected runs know
nothing of (no boundaries, markers, counters or sink ends) is an error.
A call with known boundaries splits its markers at its return for the
default summary and `--phase`: those stamped up to `return_us` are its
phases, the last ending at the return, and the later ones are its tail,
points that are not timed call work. `--durations` pairs a call's
`_START`/`_END` markers over its whole stream, as before: a span names its
own two ends and needs no boundary. Views that read
one process timeline (the default summary, `--stalls`, and any `--phase`)
refuse `--run all`, and a `--phase` is resolved, and refused if it does not
resolve, before a view reports an empty result. With `--counters`, a phase
keeps only the counters of the stream it resolved in.

The marker protocol is convention-free. The sidecar timeline model applies
these conventions to views that interpret marker names; `--markers` still
prints the raw events:

| View | Marker interpretation |
|---|---|
| default summary / `--compare` | the process stream only: each marker opens a segment running to the **next** marker (no `_START`/`_END` meaning), the last to just past the last sample, or zero-length when its marker is stamped after the last sample (a convention: the run's wall is not stored with its samples, so nothing observed bounds it; `--phase` on that marker gets the same interval); with no markers, the sampled run is one `(all)` segment. The default summary (never `--compare`) then lists each call stream (any call with boundaries, markers or sink ends) with no resource columns, since samples are process-wide: for a call with boundaries first a `type: call` record (entry, return and seal readings, its wall, its sink's account by its own document, the terminal record that arrived or `null`, and the count of sealing emissions), then its phases (`type: call_phase`), the last ending at the return, and its tail (`type: call_tail`); without boundaries a call's last interval has no known end (`end_us: null`). With `--call` only that call prints. A run with no samples still lists its calls |
| `--markers` | raw marker events in arrival order, each with its `run` and, when tagged, `call`; no pairing or segment projection |
| `--phase FOO` | resolved in one stream of one run - the process stream, or the `--call` stream - first match in time order: exact marker name first (that marker to the next), then a completed `FOO_START`..`FOO_END` span, then the first marker name containing `FOO` (that marker to the next). For a call with boundaries only its phases match, the last ending at the return, and a name only its tail has is an error; without boundaries a match on a call's last marker has no known end and is an error. With `--samples`/`--stat`, `--call` needs `--phase` and the samples stay process-wide |
| `--durations` | per `(run, call)` stream, each `FOO_START` pairs with the next later `FOO_END`; unmatched starts and markers not used by a span render as standalone points. Overlapping same-name spans in one stream pair by that rule, which is deterministic but does not identify which operation ended. `--human` collapses names seen more than once in a stream into a single aggregate row (`WAIT_P2_SEND x287  total 4200.0ms  min/avg/max`), sorted by total desc, under a "Repeated spans" heading - the split is on observed cardinality, not on the name, so nothing here knows what `WAIT_` means. Totals accumulate in microseconds so a crowd of sub-millisecond spans still sums correctly. Several streams print under a heading each. JSONL keeps one record per span and standalone point, with `run` and `call`. |
| `--stop` | process stream only: a call's marker never stops the scenario. `FOO_END` matches verbatim; `-FOO` resolves to `FOO_END`; bare `FOO` first checks for a marker named `FOO`, then falls back to `FOO_END`. Brokkr prints the resolved name whenever it differs from the requested spelling. |

Every chronological projection (segments, spans, phase lookup, and a
marker-span wall source such as ratatoskr's `SYNC_START`..`SYNC_END`) orders
a stream's markers by timestamp, arrival order breaking ties: a thread stamps
a marker before it competes for the write. The marker-span wall reads the
process stream only.

Markers are reserved for the small set of **true phase boundaries** - a
high-frequency span in the marker stream drowns every phase-oriented view at
once (each marker is a boundary). Accumulated blocking time is a *counter*
concept instead:

| View | Counter interpretation |
|---|---|
| `--stalls` | rolls up `*_wait_ns` counters per stream: max value per name (strictly-monotonic, so max == cumulative total), category = name minus the `_wait_ns` suffix, reported as ms + `% of wall` for the process stream, as ms + `%` of the call's own wall (return minus entry) for a call with boundaries, and as ms alone for any other call stream (the process wall would understate a short call). The observed max is a best-effort reading, not the call's total; a library measurement's call rows carry the call's authoritative snapshot (`brokkr man library`, Measuring a scenario). The `%` can exceed 100 for waits accumulated across concurrent threads (it's the avg threads parked in that category; single-threaded waits read as clean sub-100%). Unifies both projects - it picks up pbfhogg's `pipeline_*_wait_ns` and elivagar's `sort_chunk_write_wait_ns` etc. in one table. |
| `--counters` | prints every counter point verbatim (`t=<sec> name=value`, `[call]` before the name when tagged; JSONL carries `run` and `call`), no aggregation. `--grep <SUBSTR>` keeps only counters whose name contains the substring, and composes with `--phase`; a run emitting a progress counter every 64 blobs otherwise buries the lines that matter |

See the README "Sidecar conventions" section for the emitter-side contract.

## How `brokkr sidecar` renders

**JSONL is the default** for every view (machine/LLM consumption); `--human`
switches to fixed-width tables. Rendering lives in `src/sidecar_fmt.rs`, `--compare`'s in
`src/sidecar_fmt/compare.rs`.

- **Provenance header** (`print_run_info`) is written to **stderr** so it never
  contaminates the stdout JSONL: run timestamp/PID/command/mode/dataset, git
  commit + wall time, non-zero exit code (with signal name, e.g. SIGKILL/OOM),
  and the recorded binary path + xxh128 with a live match-check against disk.
- **Per-phase summary** (default view): a `summary` record then one `phase`
  record per segment. Human table columns: Phase, Duration, Peak RSS, Peak
  Anon, Peak Mflt, Disk Read, Disk Write, Avg Cores, plus an indented
  continuation line (user/kern cores, majflt, minflt, vol_cs, nonvol_cs,
  peak_threads). Phases with **zero in-phase samples** (shorter than the 100ms
  cadence) are dropped from the table but kept in JSONL with `avg_cores: null`.
- **Avg cores** decodes `utime+stime` jiffy deltas against
  `sysconf(_SC_CLK_TCK)` (read at runtime, not assumed 100) over the phase's
  sample span; too-short spans render `-` (table) / `null` (JSON).
- **Deltas are clamped to >= 0** (`Running::delta`, `phase_stats`) because
  cumulative `/proc` counters could regress if the process exits mid-read.
- `--compare` pairs the two runs' phases (the default summary's segments, best
  run each) by **ordinal within a name**: the k-th segment named N on A against
  the k-th named N on B, counted in time order over every segment, zero-length
  and sampleless ones included, so per-partition or per-pass repeats compare in
  order. It is a correspondence convention, not identity recovery: a missing
  emission shifts the later ordinals of its name, and same-name operations
  that reorder pair different work. Every segment of either run lands in
  exactly one row; a segment the other side lacks (a surplus repeat, a name it
  never emitted) gets a one-sided row, `(no match)` in the table, `null` in
  JSONL. Rows follow A's order; a B-only segment sits after the row holding
  B's latest paired segment before it (first if there is none), in B order
  within that slot, so when pairs cross the B-only rows need not read in B's
  order. `delta_pct` is B's duration against A's from microseconds, `null`
  (table `-`) unless both are positive. The table adds a `#` column (the
  ordinal, blank for a name that occurs once on each side that has it) when any name
  repeats. JSONL: a `compare_summary` record (`a_uuid`, `b_uuid`, `a_phases`,
  `b_phases`), then one `compare_phase` record per row (`name`, `occurrence`,
  `a`, `b`, `delta_pct`); each non-null side carries `index`, its zero-based
  position among that run's `type: phase` records in the default JSONL view,
  and `start_us`. `--counters` prints `t=<sec> name=value` (human) or one JSON
  object per counter.

## Run lifecycle: sampling, stop, kill, OOM

- **Sampling cadence** defaults to 100ms (`MeasurementTunables.sample_interval`)
  and is injectable per sidecar run. The shared supervisor schedules each tick
  against monotonic time so the ~30us of /proc read overhead does not accumulate
  drift. Each tick reads
  `/proc/<pid>/{stat,io,status}`.
- **The wall is the exit, not a tick.** Between ticks the supervisor waits on
  the child's pidfd, so a natural exit wakes it at once and the wall is taken
  there; a short run's wall is not rounded up to the next multiple of the
  sample interval. The exit wake still drains the FIFO, so a stop marker
  written before the exit wins as before, but takes no `/proc` sample, so no
  sample postdates the wall. Where a pidfd cannot be opened the exit is found
  on the next tick. Rows recorded before this change carry the tick-quantized
  wall: up to one sample interval (100 ms by default) late, always upward, most
  visible at `--bench 1` on short runs.
- **`--stop <marker>`** SIGKILLs the whole child **process group**
  (`process::signal_process_group`, so descendants are reaped too) the moment the marker
  lands. That SIGKILL is *not* treated as a failure - `stopped_by_marker`
  flags it and the run records normally, so you can bench one phase in
  isolation. The results row keeps the two facts apart: `stop_marker` is the
  marker that actually fired (shown as `killed at "..."`), `stop_marker_requested`
  the `--stop` spelling as given. A requested stop whose marker never appeared
  leaves `stop_marker` empty - the child ran to its natural end. Rows recorded
  before schema v20 only ever stored the request, so their `stop_marker` was
  moved to `stop_marker_requested` and cleared: whether those stops fired is
  unknown.
- **Deadlines**: a caller may bound each child's wall time
  (`run_sidecar_with_deadline`, which the capture helpers pass through). Past it the child's process
  group is SIGKILLed, `stopped_by_deadline` is set, and the run fails. The
  deadline bounds the child's whole life, a stop marker included: a child
  still running when it passes is killed as overdue even with a stop marker
  queued, and a stop's SIGTERM grace ends at it. The
  ratatoskr sync bench uses the script's `ceiling:`. Paths with no deadline
  run until the child
  exits or `brokkr kill`.
- **`brokkr kill` asks the holder over its control socket.** Every fresh
  hold binds `~/.brokkr/lock/<acquisition id>.sock` (`src/lock_service.rs`),
  the id recorded in the lock file. A stop request naming the live
  acquisition is accepted by the holder itself: it sets a sticky stop flag
  that no guard clears and that refuses every further lock acquisition and
  build admission, answers `accepted`, then sends itself SIGTERM. The answer
  goes first because outside a guard that SIGTERM kills the holder at once.
  "Accepted" means that commitment, not completed termination. Because
  nothing crosses a PID namespace, this works from the host against a
  sandboxed holder and between sibling sandboxes (codex and Claude Code run
  each command in its own PID namespace). A request that was delivered but
  got no well-formed answer is reported as unconfirmed: `kill` watches the
  lock for a few seconds and says whether the holder let go, and never
  follows up with a signal of its own (a second SIGTERM would escalate a
  holder already handling the first). Only when the request never reached
  the socket (an older brokkr, a missing socket) does `kill` fall back to a
  numeric SIGTERM, and only when the holder's recorded PID and time
  namespaces match the caller's and `/proc` numbers processes for the
  caller's own namespace. `brokkr lock` asks the same socket for the
  holder's own account of itself (invocation, how long it has held, its own
  CPU and RSS, run progress, its open activities, its processes and the last
  sidecar marker - see `brokkr man hygiene lock`), and both it and the
  lock wait print an advisory line when the holder recorded the caller's own
  `CODEX_THREAD_ID`.
- **SIGTERM and Ctrl-C (SIGINT)** are caught by a
  `SigtermGuard` (`src/shutdown.rs`), installed for every tracked-child
  window: each sidecar run, a passthrough child, every cargo build, an
  orchestrator's whole run (the ratatoskr sync bench holds one across all its
  iterations), a measured command's whole run (build, tool materialization,
  prepare steps and every sample), the whole of `check`/`test`/`clippy`/`deps`,
  and the other commands that run captured children with no sidecar: the
  downloads, `ingest`, `update`, the elivagar archive tools, `ocean-build`,
  `verify`, `visual`, `prepare` and nidhogg `serve` from the spawn through
  the readiness poll (an interrupt there stops the half-started server). A loop over such children stops at the
  first `Interrupted` rather than recording it as that step's failure. Guards
  nest: the outermost installs and
  restores the handlers, so an inner guard (the sidecar's, inside an
  orchestrator's) can neither clear a request that already landed nor uncover
  the outer window when it drops. A second signal while a request is still
  pending takes the default action, so a wait that never polls the flag cannot
  swallow the interrupt. Outside every guard SIGTERM terminates brokkr by
  default, except that registered test process groups are SIGKILLed first.
  On catch in the sidecar window, the child is killed, the **partial** sidecar
  data is flushed under a fresh UUID + the `dirty` alias, and the run returns
  `Interrupted`; the runner discards the attempt's own scratch output and
  brokkr exits 130 (`brokkr man clean after-an-interrupt`).
- **`brokkr kill --hard`** runs no handler: it SIGKILLs the recorded child,
  mock servers and every identity-checked descendant of brokkr, then brokkr.
  Nothing is flushed; follow up with `brokkr clean`. It signals by PID, so it
  refuses unless the holder shares the caller's PID and time namespaces (the
  same check as the fallback above); against a sandboxed holder, use the
  graceful `kill` or stop the session that owns it. Delivery failures (EPERM)
  are reported, never counted as sent. Known gaps, unchanged: a process
  group is signalled with `kill(-pid)` after a starttime re-check, and
  starttime is a clock-tick token, not a unique generation number. A brokkr SIGKILLed by
  anything else (the OOM killer) gets no walk at all; only the test runners'
  processes have a backstop for that case (a parent-death signal, and the
  orphan reap at the next locked command - see `brokkr man hygiene strays`).
- **OOM / crash preservation**: sidecar data is stored even when the child is
  OOM-killed, segfaults, or exits non-zero - the `/proc` trajectory up to the
  kill is the whole point. This holds on every sidecar-enabled harness loop, `--hotpath`/
  `--alloc` included. Ratatoskr's marker-span source keeps every reaped
  iteration's data even when the child succeeded and only the step after it
  (log write, `summary.json`) failed. Failed/dirty runs get a random UUID and update the
  `dirty` latest pointer, so `brokkr sidecar dirty` / `brokkr results dirty`
  resolve the most recent unstored run (unless that pointer write failed,
  which is warned about); the pointer is all that moves, and an earlier one
  stays readable by its own id. The child is also marked as the
  kernel OOM killer's preferred target (`src/oom.rs`) so a memory blow-up
  takes the benchmarked process, not the host.

## Trajectory capture limits

Samples, markers, counters, and FIFO records have explicit limits in
`MeasurementTunables`. The sample, marker, and counter budgets apply across
all iterations in one measurement, so a large `--bench N` request cannot
multiply the retained trajectory without bound. A library measurement
(`--bench`, `--hotpath` or `--alloc`) is the exception (`brokkr man library`, Measuring a scenario): each of its
processes is verified whole and has the budgets a verification has, and
the bound is instead on memory, since each process's trajectory is stored
as soon as it is judged and brokkr holds at most one; the stored volume
grows with N, as it does not for other measurements. Overlong FIFO records are
discarded through their newline without growing the temporary line buffer.
Reading the FIFO is bounded too: each supervisor tick consumes at most 4 MiB
of it, discarded bytes included, so a child writing without pause cannot
hold off its deadline or a stop; a record the budget cuts resumes on the
next tick. After the child ends, one last drain reads at most 16 MiB; a
child's descendant still writing past that leaves records possibly unread,
which counts as an omission like the limits above. A sealing emission
counts against the counter limit and a terminal record against the marker
limit. A FIFO read error is an omission too.
When anything is omitted, the run warns naming each cause (rejected lines,
records possibly unread, the marker or counter limit, the sample limit, cut
child output) and stores a truncation flag beside that iteration's sidecar
rows; querying an affected run with `brokkr sidecar` warns that the stored
trajectory is incomplete. A library verification tells the causes apart: only
those that may have lost a FIFO record make a call stream incomplete.

The shared child supervisor also bounds captured stdout and stderr: 16 MiB per
stream for ordinary commands and sidecar measurements, 64 MiB for the ratatoskr
harness and piners corpus report, and 128 MiB for Cargo output. The
`[cargo_env]` evaluator, `script_check` and a `brokkr library` verification
run (whose harness must write nothing, so 64 KiB of each is kept for the
report) have smaller contract-specific limits.
The supervisor keeps reading after a limit so the child cannot block on a full
pipe. A captured command that exceeds its limit fails with a capture error rather
than returning partial bytes to a result parser. A sidecar measurement retains
the bounded bytes, marks its trajectory incomplete, and fails that iteration
when a child stream exceeds its limit.

## Sidecar backup & rotation

After each stored run (while the bench lock is still held), `backup_sidecar`
snapshots sidecar.db to `$XDG_DATA_HOME/brokkr/sidecar-backups/<root key>/`
(falling back to `~/.local/share/...`), with rotation. The default
`MeasurementTunables.sidecar_backup_copies` value is 3, so it keeps
`sidecar.db` (newest), `sidecar.db.1`, and `sidecar.db.2`.

The rotation is per project root, the directory whose `.brokkr/sidecar.db` is
copied, so two checkouts of one project (`~/Programs/pbfhogg` and a copy under
`PRs/`) never evict each other's copies. The root key is the canonical root's
directory name, cut to 64 bytes, then `-` and a 16-hex-digit hash of the
canonical path. Beside the copies, `origin` holds that canonical path and
asserts ownership: a backup whose root does not match it is refused, as is one
into a directory holding copies but no `origin` (restore the file or move the
directory aside), and a slot or `origin` that is not a regular file (a
symlink, say) refuses too. A refusal is a backup failure: warned, the copies
untouched. A moved or renamed checkout starts a new rotation and the old one
stays, named by its `origin`. Nothing prunes these; `brokkr env` reports their
size.

An earlier brokkr keyed one rotation per project kind
(`<kind>-sidecar.db`, `.1`, `.2` directly under `sidecar-backups/`), shared by
every checkout of the kind. Those copies are left alone, since one may be the
only copy of a checkout that is gone; only their unfinished staging file,
`.<kind>-sidecar.db.partial`, is removed by each backup of that kind.

The sequence is crash-safe, in that no crash leaves a partial primary:
SQLite's **online backup API** writes a self-contained DELETE-journal copy to
a temp file and runs `quick_check`; older copies shift (`.1`->`.2`) and the
current primary is hard-linked to `.1`; then the temp is fsynced, atomically
renamed into the primary slot and the directory fsynced. A failure before the
final rename leaves the existing primary intact. The rotation is not one
transaction - a crash mid-rotation can lose a retention slot - and the
directory fsync is best effort: a failure of it is warned about (one the
filesystem refuses outright is not), and the backup still counts as made.
Backup failure is logged but **non-fatal** to the run.

## Hotpath JSON contract

Brokkr does not depend on the `hotpath` crate directly - it parses the JSON
report that hotpath-instrumented binaries write to `HOTPATH_OUTPUT_PATH`.
Function percentiles are read from every `p<number>` field, sorted
numerically, and may be fractional (`p99.9`). Timing values are stored in
nanoseconds, byte values in bytes, and percentages as percentages. Function
diffs show a relative change only when both totals have the same quantity
kind; a duration is never compared with an allocation size or percentage. The
results database keeps the old display-text columns and parses them when a
typed metric is missing. Existing rows are not rewritten.

Env vars brokkr sets on hotpath child processes:
`HOTPATH_METRICS_SERVER_OFF=true`, `HOTPATH_OUTPUT_FORMAT=json` and
`HOTPATH_OUTPUT_PATH`, a report path under the project's `.brokkr/`:
`capture_hotpath` uses a fixed `hotpath-report.json`, cleared before and
after; the shared measure runner gives every launch a name of its own,
declares it to the launch's reservation where there is one, and removes it
after the launch. A `brokkr library` profiled run is the
exception: its report goes in the run directory, is judged by a stricter
rule than the parse above (a called function of the requested dimension)
and is removed with the directory (`brokkr man library`, Profiling a
scenario).
