# Measurement modes: --bench, --hotpath, --alloc, --stop

Every measurable command is a top-level brokkr subcommand. Measurement mode is
a flag:

```
brokkr <command> [--bench [N] | --hotpath [N] | --alloc [N]] [command options]
```

- No flag - build, run once, print timing. Acquires lockfile, no DB storage.
- `--bench` - full benchmark: lockfile, 3 runs (or N), best-of-N stored in DB.
- `--hotpath` - function-level timing via hotpath feature. 1 run (or N).
- `--alloc` - per-function allocation tracking via hotpath-alloc feature. 1
  run (or N).
- `--stop <marker>` - kill the child when this FIFO marker is emitted. Allows
  benchmarking a specific phase in isolation. The SIGKILL exit is treated as
  success.

All measured modes automatically run a sidecar that samples `/proc` metrics at
100ms and provides `BROKKR_MARKER_FIFO` for phase markers. Sidecar data is
stored in `.brokkr/sidecar.db` (gitignored). Sidecar data is preserved even
when the child is OOM-killed.

Dataset paths resolve from `brokkr.toml` automatically. All flags go after
the command name.

## Measurable commands per project

The measurable commands are pbfhogg's data ops, elivagar's tilegen/etc., and
nidhogg's query/ingest. Piners' `corpus` is also measurable, but **only
`--hotpath`/`--alloc`** (see `docs/commands/corpus.md`): a bare `corpus` is the
parity run (gate + `runs.db`), while `corpus --hotpath`/`--alloc` builds the
`[piners.harness]` crate with the hotpath feature and records to `results.db`
via the same `BenchContext` path as everyone else. `corpus --bench` is refused
- the parity harness emits NDJSON dispositions, not the `key=value` stderr
timing contract `--bench` needs. The build-config seam both paths share is
`BenchContext::with_build_config` + `BuildConfig::for_harness`.

## Benchmark harness

`BenchHarness` (in `src/harness.rs`) provides:
- Exclusive lockfile (prevents parallel bench/verify/hotpath runs)
- SQLite result storage with git commit, hostname, env snapshot
- `run_internal(config, closure)` - in-process timing (N runs, min/avg/max).
  Does **not** store sidecar data (no store step).
- `run_hotpath(config, program, closure)` - like `run_internal`, but the
  closure also returns the `SidecarData` from `run_hotpath_capture`, which is
  persisted to sidecar.db under the recorded UUID. This is what makes
  `brokkr sidecar <uuid>` work for `--hotpath`/`--alloc`; the raw
  `run_hotpath_capture` returns the data but stores nothing.
- `run_external(config, binary, args)` - subprocess timing
- `run_distribution(config, closure)` - distribution timing
  (min/p50/p95/max)

Results in `.brokkr/results.db` per project (gitignored).

## Sidecar profiler

The sidecar is always-on for all measured modes. It samples `/proc/{pid}/stat`,
`/proc/{pid}/io`, and `/proc/{pid}/status` at 100ms intervals and reads phase
markers from a FIFO. All data is buffered in memory during the run and
bulk-inserted to `.brokkr/sidecar.db` (gitignored) after the child exits.
Results DB (`.brokkr/results.db`) stays small and git-tracked.

Key files: `src/sidecar.rs` (core), `src/harness.rs` (`run_external`,
`run_external_with_kv`, `run_hotpath_capture` - all sidecar-enabled; the
hotpath/alloc data reaches sidecar.db via the `run_hotpath` wrapper, since
`run_hotpath_capture` itself only returns the payload), `src/db/sidecar.rs`
(`SidecarDb`).

The child process receives `BROKKR_MARKER_FIFO` env var pointing to a named
pipe. Stdout/stderr are drained in background threads to prevent pipe-buffer
deadlock. Child exit is detected via `try_wait()` and the exact exit time is
recorded for wall-clock measurement. Sidecar data is stored even when the
child fails (OOM, signal, non-zero exit).

## Querying sidecar data

`brokkr sidecar <uuid>` - bare form is the per-phase JSONL summary (pass
`--human` for a table). View selectors are mutually exclusive:
- `--samples` - raw JSONL /proc samples
- `--markers` - raw JSONL marker events
- `--durations` - START/END pair timings
- `--counters` - application counters
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
| `vsize` | stat field 22 | virtual size (bytes) |
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

## Marker & counter rules

The FIFO carries two line types (parsed in `SidecarFifo::drain`,
`src/sidecar.rs`):

- **Marker** - `<ts_us> <name>`. Assigned a monotonic `marker_idx` in arrival
  order; the last name seen is also mirrored to a status file so `brokkr lock`
  can show the live phase. Markers are point-in-time bookmarks - the protocol
  itself knows nothing about spans or pairs.
- **Counter** - `<ts_us> @<name>=<value>`. The value **must parse as `i64`**
  or the line is silently dropped. `<ts_us>` that doesn't parse is skipped for
  either type. Timestamps are microseconds since process start.

How marker *names* are interpreted is per query mode - the raw protocol is
convention-free, but individual views opt into naming conventions:

| View | Marker interpretation |
|---|---|
| default summary / `--markers` / `--phase` | each marker opens a segment running to the **next** marker (no `_START`/`_END` meaning); `--phase FOO` matches exact, then `FOO_START`..`FOO_END`, then substring |
| `--durations` | pairs `FOO_START` with the next `FOO_END`; unpaired starts render as standalone |
| `--stop` | three spellings resolve to one marker: verbatim `FOO_END`; `-FOO` -> `FOO_END`; bare `FOO` -> `FOO_END` (fallback prints a notice) |

Markers are reserved for the small set of **true phase boundaries** - a
high-frequency span in the marker stream drowns every phase-oriented view at
once (each marker is a boundary). Accumulated blocking time is a *counter*
concept instead:

| View | Counter interpretation |
|---|---|
| `--stalls` | rolls up `*_wait_ns` counters: max value per name (strictly-monotonic, so max == cumulative total), category = name minus the `_wait_ns` suffix, reported as ms + `% of wall`. The `%` can exceed 100 for waits accumulated across concurrent threads (it's the avg threads parked in that category; single-threaded waits read as clean sub-100%). Unifies both projects - it picks up pbfhogg's `pipeline_*_wait_ns` and elivagar's `sort_chunk_write_wait_ns` etc. in one table. |
| `--counters` | prints every counter point verbatim (`t=<sec> name=value`), no aggregation |

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

- **Sampling cadence** is a fixed 100ms (`SAMPLE_INTERVAL_US`), driven by
  `clock_nanosleep(TIMER_ABSTIME, CLOCK_MONOTONIC)` so the ~30µs of /proc read
  overhead per tick doesn't accumulate drift. Each tick reads
  `/proc/<pid>/{stat,io,status}`.
- **`--stop <marker>`** SIGKILLs the whole child **process group**
  (`send_signal_pgrp`, so descendants are reaped too) the moment the marker
  lands. That SIGKILL is *not* treated as a failure - `stopped_by_marker`
  flags it and the run records normally, so you can bench one phase in
  isolation.
- **`brokkr kill` (SIGTERM)** is handled by a `SigtermGuard` scoped to the
  sidecar window only (outside it, SIGTERM falls through to default terminate
  - there's no child to reap during `cargo build`/`check`). On catch, the
  child is killed, the **partial** sidecar data is flushed under a fresh
  UUID + the `dirty` alias, and the run returns `Interrupted`.
- **OOM / crash preservation**: sidecar data is stored even when the child is
  OOM-killed, segfaults, or exits non-zero - the `/proc` trajectory up to the
  kill is the whole point. Failed/dirty runs get a random UUID and update the
  `dirty` latest pointer, so `brokkr sidecar dirty` / `brokkr results dirty`
  always resolve the most recent unstored run. The child is also marked as the
  kernel OOM killer's preferred target (`src/oom.rs`) so a memory blow-up
  takes the benchmarked process, not the host.

## Sidecar backup & rotation

After each stored run (while the bench lock is still held), `backup_sidecar`
snapshots sidecar.db to `$XDG_DATA_HOME/brokkr/sidecar-backups/` (falling back
to `~/.local/share/...`), named `<project>-sidecar.db` with rotation - this is
the one place the project name enters the sidecar path. It keeps
`SIDECAR_BACKUP_COPIES` (3) generations: `.db` (newest), `.db.1`, `.db.2`.

The sequence is crash-safe: SQLite's **online backup API** writes a
self-contained DELETE-journal copy to a temp file, runs `quick_check`, fsyncs
it, shifts older copies (`.1`->`.2`), hard-links the current primary to `.1`,
then atomically renames the temp into the primary slot and fsyncs the
directory. A failure before the final rename leaves the existing primary
intact. Backup failure is logged but **non-fatal** to the run.

## Hotpath JSON contract

Brokkr does not depend on the `hotpath` crate directly - it parses the JSON
report that hotpath-instrumented binaries write to `HOTPATH_OUTPUT_PATH`.
See the module header on `src/db/hotpath.rs` for the percentile-column
constraint (p50/p95/p99 hardcoded; custom percentiles like p99.9 are silently
dropped).

Env vars brokkr sets on hotpath child processes:
`HOTPATH_METRICS_SERVER_OFF=true`, `HOTPATH_OUTPUT_FORMAT=json`,
`HOTPATH_OUTPUT_PATH=<scratch>/hotpath-report.json`.
