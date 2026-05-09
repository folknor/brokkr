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

## Benchmark harness

`BenchHarness` (in `src/harness.rs`) provides:
- Exclusive lockfile (prevents parallel bench/verify/hotpath runs)
- SQLite result storage with git commit, hostname, env snapshot
- `run_internal(config, closure)` - in-process timing (N runs, min/avg/max)
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
`run_external_with_kv`, `run_hotpath_capture` - all sidecar-enabled),
`src/db/sidecar.rs` (`SidecarDb`).

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
most recent failed/dirty-tree run.

## Hotpath JSON contract

Brokkr does not depend on the `hotpath` crate directly - it parses the JSON
report that hotpath-instrumented binaries write to `HOTPATH_OUTPUT_PATH`.
See the module header on `src/db/hotpath.rs` for the percentile-column
constraint (p50/p95/p99 hardcoded; custom percentiles like p99.9 are silently
dropped).

Env vars brokkr sets on hotpath child processes:
`HOTPATH_METRICS_SERVER_OFF=true`, `HOTPATH_OUTPUT_FORMAT=json`,
`HOTPATH_OUTPUT_PATH=<scratch>/hotpath-report.json`.
