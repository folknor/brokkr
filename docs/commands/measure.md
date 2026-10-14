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
nidhogg's query/ingest. Piners' `corpus` is also measurable, but **only
`--hotpath`/`--alloc`** (see `docs/commands/corpus.md`): a bare `corpus` is the
parity run (gate + `runs.db`), while `corpus --hotpath`/`--alloc` builds the
`[piners.harness]` crate with the hotpath feature and records to `results.db`
via the same `MeasurementSession` path as everyone else. `corpus --bench` is refused
- the parity harness emits NDJSON dispositions, not the `key=value` stderr
timing contract `--bench` needs. The build-config seam both paths share is
`MeasurementSession::with_build_config` + `BuildConfig::for_harness`.

## Command plans

Pbfhogg, elivagar and nidhogg use `src/measure/runner.rs`. Binary plans run a
built executable; external plans materialize tools outside the measured samples;
HTTP/server lifecycle plans provide their own protocol operations. All use one
`MeasurementSession` for the lock, paths, optional build and request provenance.

Dry-run validates inputs and argv without acquiring a measurement session,
building, downloading tools, starting servers, or creating run artifacts.
It still follows the command's lock policy, so a measured command can wait for
the brokkr lock before it reports the plan.
Input hash verification may refresh `.brokkr/hash_cache`; this validation cache
is the sole permitted planning write. Elivagar's argv construction creates no
directories. API dry-run derives queries without contacting the running server.

Run executes once without persisting result or sidecar records. Stored modes
use the request's measurement mode and brokkr invocation; per-row fallback
fields do not exist. Row command names and subprocess `cli_args` retain their
previous spelling (examples and HTTP/server lifecycles retain absent `cli_args`).

Declared sample scratch is reset before each sample and removed after success.
A failed process, capture, or artifact promotion retains its current output.
Shared input caches, downloaded tools and durable output archives are distinct
from sample scratch. Tilegen's checkpoint directory remains available for
`--skip-to`; elivagar owns its checkpoint reset contract.

## Benchmark harness

`BenchHarness` (in `src/harness.rs`) provides:
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
  a sidecar. `run_hotpath(config, program, closure)` uses the same loop with a
  FIFO; `capture_hotpath` receives that FIFO and uses the session's held lock
  and stop marker. Missing or invalid profile JSON fails the iteration while
  retaining its captured evidence.
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
  when the row could not be stored).
- `[run] results.db a1b2c3d4`, the stored row's short id - the one line
  naming it, and not hidden by quiet mode, since it is how the run is found
  again (`brokkr results a1b2c3d4`, `brokkr sidecar a1b2c3d4`).
- one `[sidecar]` summary line on stderr (quiet-gated) once the profile is
  stored: the best run's sample, marker and counter counts, with
  `best run k/n:` when there were several (`(sidecar.db)`). The profile shares
  the results row's id, so the line does not repeat it. A trajectory with no
  results row is filed under the `dirty` alias, and only then does the line
  name its own id (`sidecar.db a1b2c3d4, filed as dirty`).

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
`src/harness_mod/measurement_loop.rs` (iteration collection, capture and recording),
`src/harness_mod/types_run.rs` (session provenance and row construction), and
`src/db/sidecar.rs` (`SidecarDb`).

The child process receives `BROKKR_MARKER_FIFO` env var pointing to a named
pipe. The shared supervisor in `src/output/child.rs` polls the child, handles
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
- `--stat <field>` - min/max/avg/p50/p95
- `--compare <a> <b>` - phase-aligned

Filter flags `--phase`, `--range`, `--where` compose with `--samples` and
`--stat`; `--fields`/`--every`/`--head`/`--tail` only with `--samples`. A UUID
is required except for `--compare`; the `dirty` pseudo-UUID resolves to the
most recent failed/dirty-tree run (see "Run lifecycle" below for how it is
set). `--bench N` stores **all N runs** in sidecar.db but marks
`best_run_idx` (the run whose wall-clock matched the reported `elapsed_ms`);
the default view and `--stat`/`--phase` read that best run unless another
`run_idx` is selected.

The UUID argument is a prefix, matched literally (`_`/`%` are not wildcards),
and it must name **one** session: a prefix shared by several sessions is an
error listing the candidates rather than a merged timeline, and an empty
prefix is refused.

## Sample fields

Each 100ms sample is assembled by `read_proc_metrics` from three files, and
**all three reads must succeed or the whole sample is dropped** (a partial
read at process exit would corrupt phase deltas). Field names below are the
tokens accepted by `--fields`, `--where`, and `--stat`.

| Field | Source | Meaning |
|---|---|---|
| `i` | - | sample index |
| `t` | - | seconds since process start (always present in `--samples` JSON) |
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
  percentiles, matching `harness::percentile`).

A `--where` that does not parse, or a field name outside the table above in
`--where`/`--fields`/`--stat`, is an error before anything prints - never a
silently skipped filter or dropped column.

## Marker & counter rules

The FIFO carries two line types (parsed in `SidecarFifo::drain`,
`src/sidecar.rs`):

- **Marker** - `<ts_us> <name>`. Assigned a monotonic `marker_idx` in arrival
  order; the last name seen is also mirrored to a status file so `brokkr lock`
  can show the live phase. That file has one location on every harness path -
  `~/.brokkr/sidecar-status`, beside the global lock (`sidecar::status_path`) -
  and carries the writer's PID, so `brokkr lock` ignores one left behind by an
  earlier holder. Markers are point-in-time bookmarks - the protocol
  itself knows nothing about spans or pairs. The name is untrusted text; every
  JSONL view encodes it with serde_json, so control characters are escaped.
- **Counter** - `<ts_us> @<name>=<value>`. The value **must parse as `i64`**
  or the line is silently dropped. `<ts_us>` that doesn't parse is skipped for
  either type. Timestamps are microseconds since process start.

The marker protocol is convention-free. The sidecar timeline model applies
these conventions to views that interpret marker names; `--markers` still
prints the raw events:

| View | Marker interpretation |
|---|---|
| default summary / `--compare` | each marker opens a segment running to the **next** marker (no `_START`/`_END` meaning); with no markers, the sampled run is one `(all)` segment |
| `--markers` | raw marker events in arrival order; no pairing or segment projection |
| `--phase FOO` | exact marker name first (that marker to the next), then a completed `FOO_START`..`FOO_END` span, then the first marker name containing `FOO` (that marker to the next) |
| `--durations` | each `FOO_START` pairs with the next later `FOO_END`; unmatched starts and markers not used by a span render as standalone points. `--human` collapses names seen more than once into a single aggregate row (`WAIT_P2_SEND x287  total 4200.0ms  min/avg/max`), sorted by total desc, under a "Repeated spans" heading - the split is on observed cardinality, not on the name, so nothing here knows what `WAIT_` means. Totals accumulate in microseconds so a crowd of sub-millisecond spans still sums correctly. Collapsing is **off under `--run all`**: markers carry no `run_idx`, so a once-per-run phase would otherwise look like a repeated span and be aggregated away. JSONL keeps one record per span and standalone point. |
| `--stop` | `FOO_END` matches verbatim; `-FOO` resolves to `FOO_END`; bare `FOO` first checks for a marker named `FOO`, then falls back to `FOO_END`. Brokkr prints the resolved name whenever it differs from the requested spelling. |

Markers are reserved for the small set of **true phase boundaries** - a
high-frequency span in the marker stream drowns every phase-oriented view at
once (each marker is a boundary). Accumulated blocking time is a *counter*
concept instead:

| View | Counter interpretation |
|---|---|
| `--stalls` | rolls up `*_wait_ns` counters: max value per name (strictly-monotonic, so max == cumulative total), category = name minus the `_wait_ns` suffix, reported as ms + `% of wall`. The `%` can exceed 100 for waits accumulated across concurrent threads (it's the avg threads parked in that category; single-threaded waits read as clean sub-100%). Unifies both projects - it picks up pbfhogg's `pipeline_*_wait_ns` and elivagar's `sort_chunk_write_wait_ns` etc. in one table. |
| `--counters` | prints every counter point verbatim (`t=<sec> name=value`), no aggregation. `--grep <SUBSTR>` keeps only counters whose name contains the substring, and composes with `--phase`; a run emitting a progress counter every 64 blobs otherwise buries the lines that matter |

See the README "Sidecar conventions" section for the emitter-side contract.

## How `brokkr sidecar` renders

**JSONL is the default** for every view (machine/LLM consumption); `--human`
switches to fixed-width tables. Rendering lives in `src/sidecar_fmt.rs`.

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
- `--compare` aligns run B's phases to run A by name and appends a `delta_pct`
  on duration. `--counters` prints `t=<sec> name=value` (human) or one JSON
  object per counter.

## Run lifecycle: sampling, stop, kill, OOM

- **Sampling cadence** defaults to 100ms (`MeasurementTunables.sample_interval`)
  and is injectable per sidecar run. The shared supervisor schedules each tick
  against monotonic time so the ~30us of /proc read overhead does not accumulate
  drift. Each tick reads
  `/proc/<pid>/{stat,io,status}`.
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
  ratatoskr sync bench uses the script's `ceiling:`; measured `corpus` uses the
  parity path's hang backstop. Paths with no deadline run until the child
  exits or `brokkr kill`.
- **`brokkr kill` asks the holder over its control socket.** Every fresh
  hold binds `~/.brokkr/lock/<acquisition id>.sock` (`src/lock_service.rs`),
  the id recorded in the lock file. A stop request naming the live
  acquisition is accepted by the holder itself: it sets a sticky stop flag
  that no guard clears and that refuses every further lock acquisition and
  build admission, then sends itself SIGTERM. "Accepted" means that
  commitment, not completed termination. Because nothing crosses a PID
  namespace, this works from the host against a sandboxed holder and between
  sibling sandboxes (codex runs each command in its own PID namespace). Only
  when the socket does not answer (an older brokkr, a stopped holder) does
  `kill` fall back to a numeric SIGTERM, and only when the holder's recorded
  PID and time namespaces match the caller's and `/proc` numbers processes
  for the caller's own namespace. `brokkr lock` asks the same socket for the
  holder's own account of itself (invocation, how long it has held, CPU, RSS,
  progress), and both it and the lock wait print an advisory line when the
  holder recorded the caller's own `CODEX_THREAD_ID`.
- **SIGTERM and Ctrl-C (SIGINT)** are caught by a
  `SigtermGuard` (`src/shutdown.rs`), installed for every tracked-child
  window: each sidecar run, a passthrough child, an orchestrator's whole run
  (the ratatoskr sync bench holds one across all its iterations), and the
  whole of `check`/`test`/`clippy`. Guards nest: the outermost installs and
  restores the handlers, so an inner guard (the sidecar's, inside an
  orchestrator's) can neither clear a request that already landed nor uncover
  the outer window when it drops. A second signal while a request is still
  pending takes the default action, so a wait that never polls the flag cannot
  swallow the interrupt. Outside every guard SIGTERM terminates brokkr by
  default, except that registered test process groups are SIGKILLed first.
  On catch in the sidecar window, the child is killed, the **partial** sidecar
  data is flushed under a fresh UUID + the `dirty` alias, and the run returns
  `Interrupted`; `main` runs the scratch cleanup and exits 130.
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
  orphan reap at the next locked command - see `brokkr man check strays`).
- **OOM / crash preservation**: sidecar data is stored even when the child is
  OOM-killed, segfaults, or exits non-zero - the `/proc` trajectory up to the
  kill is the whole point. This holds on every sidecar-enabled harness loop, `--hotpath`/
  `--alloc` included. Ratatoskr's marker-span source keeps every reaped
  iteration's data even when the child succeeded and only the step after it
  (log write, `summary.json`) failed. Failed/dirty runs get a random UUID and update the
  `dirty` latest pointer, so `brokkr sidecar dirty` / `brokkr results dirty`
  always resolve the most recent unstored run. The child is also marked as the
  kernel OOM killer's preferred target (`src/oom.rs`) so a memory blow-up
  takes the benchmarked process, not the host.

## Sidecar backup & rotation

After each stored run (while the bench lock is still held), `backup_sidecar`
snapshots sidecar.db to `$XDG_DATA_HOME/brokkr/sidecar-backups/` (falling back
to `~/.local/share/...`), named `<project>-sidecar.db` with rotation - this is
the one place the project name enters the sidecar path. The default
`MeasurementTunables.sidecar_backup_copies` value is 3, so it keeps `.db`
(newest), `.db.1`, and `.db.2`.

The sequence is crash-safe: SQLite's **online backup API** writes a
self-contained DELETE-journal copy to a temp file, runs `quick_check`, fsyncs
it, shifts older copies (`.1`->`.2`), hard-links the current primary to `.1`,
then atomically renames the temp into the primary slot and fsyncs the
directory. A failure before the final rename leaves the existing primary
intact. Backup failure is logged but **non-fatal** to the run.

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
`HOTPATH_METRICS_SERVER_OFF=true`, `HOTPATH_OUTPUT_FORMAT=json`,
`HOTPATH_OUTPUT_PATH=<scratch>/hotpath-report.json`.
