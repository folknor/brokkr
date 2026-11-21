# Measure harness

This file covers how brokkr's typed measurement harness is built: the
session facade, the measurement loop, recording, child capture and the
sidecar's moving parts. What a user sees and acts on is in
`docs/commands/measure.md` (`brokkr man measure`); this file names the types
and functions behind it. "Key files" maps the modules.

## Key files

The harness is spread over these modules:

- `src/harness.rs`: the public facade. It re-exports `BenchHarness`,
  `BenchConfig`, `BuildIdentity`, `WallSource` and `format_cli_args`.
- `src/measure.rs`: `MeasureRequest`, `MeasureMode`, `BenchHarness` and the
  `run_hotpath` and `run_external*` entry points.
- `src/measure/execution.rs`: `MeasurementLoop` and `measure`, the iteration
  collection.
- `src/measure/wall.rs`: `WallSource`.
- `src/measure/capture.rs`: child capture, `capture_hotpath` included.
- `src/measure/record.rs`: `record`, row and sidecar persistence.
- `src/output/child.rs`: the shared child supervisor.
- `src/sidecar.rs`: the `/proc` sampler, `SidecarFifo` and
  `MeasurementTunables`.
- `src/sidecar/namespace.rs`: FIFO naming.
- `src/sidecar_fmt.rs` and `src/sidecar_fmt/compare.rs`: `brokkr sidecar`
  rendering and `--compare`.
- `src/db/sidecar.rs`: `SidecarDb`.
- `src/sidecar_backup.rs`: backup rotation.
- `src/lock_service.rs`: the holder's control socket.
- `src/shutdown.rs`: `SigtermGuard`.
- `src/oom.rs`: the child's OOM-killer preference.

## BenchHarness

`BenchHarness` is the session every typed measured command opens. It holds
the lock guard for the session, opens the project's `results.db`, collects
git and host facts, and applies the dirty-tree rule: a dirty build root
without `--force` is a `DevError::Preflight`, and with `--force` a warning.

`BenchHarness::new_with_lock` resolves `results.db` and `sidecar.db` under
the project root's `.brokkr/` for every project, never a `--commit`
worktree's, and reads git provenance from the build root.

The lock is re-entrant within one process, and the flock is released when
the last guard drops. Ratatoskr's `sync --gate all` relies on this: it holds
the lock across its sweep while each swept bench acquires internally.

The mode a row records comes from the request, set once, so the `[result]`
line's `mode=` and the sidecar provenance can't disagree.

## Measurement loop

`measure(config, program, source, sidecar, closure)` collects a
`Measurement` through one `MeasurementLoop`. Its `WallSource` is fixed for
the whole run:

- external wall, timed by brokkr
- self-reported stderr timing (`elapsed_ms=`)
- a required marker span

Every iteration keeps its streams, typed ending, optional sidecar data and
observer failures. Best-of-N picks on `elapsed_us` where the loop has it;
ties keep the first run. The loop stamps its own start, which is what
`prev.gap_seconds` measures to.

A self-reported wall replaces only the iteration's elapsed fields. Its
counters, an answer's `meta.exit_code`, an observer's additions and a
hotpath profile stay.

The entry points over the loop:

- `run_hotpath(config, program, source, closure)` runs the loop with a FIFO
  and the caller's wall source, so the profile is the one attached to the
  best-ranked iteration. `capture_hotpath` receives that FIFO and uses the
  session's held lock and stop marker.
- `run_external(config, invocation, cwd)` and its `run_external_ok`,
  `run_external_prepared` and `run_external_with_kv` variants time a
  subprocess.

`Measurement` (`src/measure/execution.rs`) keeps stdout and stderr for
every captured iteration, so a caller can inspect either before recording;
storage uses the winner's metrics.

## Capture by mode

Each measured mode reaches one harness function, and the function decides
which channels are kept (the channels are described in
`docs/commands/output-channels.md`):

| Harness fn | stdout | stderr kv | FIFO | Reached from |
|---|---|---|---|---|
| `run_passthrough_timed` | inherited | no | no | run mode, no `--bench` |
| `run_external_ok` | dropped | counters only | yes | `brokkr mogwai` |
| `run_external_with_kv` | dropped | yes | yes | sluggrs `hotpath --bench` |
| `capture_hotpath` | captured | counters, JSON | yes | `--hotpath`, `--alloc` |

`run_passthrough_timed` lives in `src/output/child.rs`; the others are in
`src/measure.rs`, except `capture_hotpath` in `src/measure/capture.rs`.

The two external paths differ only in which clock supplies the recorded
`elapsed_ms`: `run_external_ok` measures the child from outside, and
`run_external_with_kv` takes the child's own `elapsed_ms=` and discards the
external wall. Both scrape non-timing stderr pairs as counters
unconditionally. Don't reintroduce scraping as an option: a separate path
that made it one differed from `run_external_ok` only by a bool the shared
body already took.

## Parsers

These functions turn a child's output into stored values:

- **`parse_kv_lines_us`** (`src/measure/kv.rs`): the stderr `key=value`
  scrape, with the field-name grammar, the reserved timing names, the
  brokkr-namespace drop and the microsecond timing reading.
- **`SidecarFifo::drain`** (`src/sidecar.rs`): see "Sidecar FIFO".
- **`format_result_line`** (`src/measure/record.rs`): renders the `[result]`
  line and computes the derived `read_mbs` and `write_mbs` fields.

## Build features

Every measured build takes its features from `MeasureRequest::build_features`,
which adds the mode's instrumentation feature, so no `--hotpath` or
`--alloc` run builds a plain child.

## Recording

`record(config, measurement, post)` postprocesses the winner and records the
results row before its sidecar. Measurement, postprocessing and row-insert
failures preserve the captured evidence under `dirty`, and storage failures
are reported. Callers never arrange a separate sidecar commit. The session is
written in one transaction after the last iteration, so a storage failure
leaves no session under an id no line named.

## Child supervisor

The shared supervisor in `src/output/child.rs` waits on the child, through a
pidfd between ticks. It handles stop markers, deadlines and interrupts, and
drains stdout and stderr with a bounded settle period. It records one typed
ending before waiting for the drains: `Exited`, `Signalled`, `StopMarker`,
`Deadline`, `Interrupted` or `WaitFailed`. It runs every captured and
passthrough subprocess, and never replays a captured stream while the child
runs; a caller that wants the stream shown replays the captured bytes
afterwards.

The sidecar supplies only the per-tick `/proc` sampler and the FIFO observer.
A stop marker SIGKILLs the child's process group through
`process::signal_process_group`.

## Sidecar FIFO

`SidecarFifo::drain` parses the FIFO's line types: markers, counters, call
streams and sink ends. Its per-line limit, `max_fifo_line_bytes` in
`MeasurementTunables`, is a defence; the producer's 512-byte single-write
rule is the contract. `drain_after_exit` and `drain_final` run the
post-exit drains under their byte budgets.

## MeasurementTunables

`MeasurementTunables` in `src/sidecar.rs` holds the measurement defaults, and
`DEFAULT_MEASUREMENT_TUNABLES` the values. A focused test or an alternate run
copies the value and overrides fields:

- `sample_interval`: the sampling cadence, 100 ms
- `sidecar_backup_copies`: backup rotation depth, 3
- `stderr_preview_chars`: stderr characters quoted when a self-reporting
  child printed no `elapsed_ms=`, 500
- `hotpath_report_filename`: the typed capture's report, `hotpath-report.json`
- `max_samples`, `max_markers`, `max_counters`: capture budgets across every
  run of one measurement
- `max_fifo_line_bytes`: bytes kept from one FIFO line

## Rendering

`--stat` percentiles come from `sidecar::linear_percentile`, which bench and
sidecar summaries share so they can't disagree on one sample set. Phase
deltas go through `Running::delta` and `phase_stats` in `src/sidecar_fmt.rs`,
which clamp to zero, because a cumulative `/proc` counter can read lower when
the process exits between reads.

The prefixed console printers live in `src/output/console.rs` and share one
renderer (see `reference/output.md`). `measure::ProfiledMode` is the only
emitter of allocation-profile notes.

## Hotpath reports

The typed hotpath capture uses the fixed `hotpath-report.json`, cleared
before and after. The shared measure runner gives every launch a report name
of its own, declares it to the launch's reservation where there is one, and
removes it after the launch.

## Control socket and guards

Every fresh hold binds `~/.brokkr/lock/ACQUISITION_ID.sock` in
`src/lock_service.rs`, which serves `stop` and status requests. Every
tracked-child window installs a `SigtermGuard` from `src/shutdown.rs`; guards
nest, and the outermost installs and restores the handlers. `src/oom.rs`
writes `1000` to the child's `oom_score_adj` before exec, ignoring errors.
