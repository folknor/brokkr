# Measure harness

This file covers how brokkr's measurement plumbing is built: child capture,
the sidecar's moving parts, and the read-only typed history. Script benches
are the only measuring path; their runner is in `src/bench/runner/` and
`reference/bench-resources.md`. What a user sees and acts on is in
`docs/commands/measure.md` (`brokkr man measure`); this file names the types
and functions behind it. "Key files" maps the modules.

## Key files

The plumbing is spread over these modules:

- `src/output/child.rs`: the shared child supervisor.
- `src/sidecar.rs`: the `/proc` sampler, `SidecarFifo` and
  `MeasurementTunables`.
- `src/sidecar/namespace.rs`: FIFO naming.
- `src/sidecar_fmt.rs` and `src/sidecar_fmt/compare.rs`: `brokkr sidecar`
  rendering and `--compare`.
- `src/db/`: `ResultsDb` and `src/db/sidecar.rs`'s `SidecarDb`, the typed
  history's readers.
- `src/lock_service.rs`: the holder's control socket.
- `src/shutdown.rs`: `SigtermGuard`.
- `src/oom.rs`: the child's OOM-killer preference.

## Typed history

No command writes `results.db` or `sidecar.db`; `brokkr results`,
`brokkr sidecar` and `brokkr invalidate` read and delete what earlier typed
runs stored. The row and session writers (`ResultsDb::insert`,
`SidecarDb::store_session`) survive only under `cfg(test)`, as the fixtures
that seed those stores for the readers' tests. A field only a fixture writer
reads carries `cfg_attr(not(test), allow(dead_code))` with a note saying so.
Don't grow them back into a production writer: a new measuring path stores
through the bench store (`reference/bench-store.md`).

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

A sidecar run is `supervise_sidecar`, which samples and drains until the
child ends and hands back an `OpenCapture`, then `OpenCapture::finish`,
which closes the capture into a `SidecarRunResult`. A script-bench call
pumps the open capture while its borrow's release waits.

## MeasurementTunables

`MeasurementTunables` in `src/sidecar.rs` holds the measurement defaults, and
`DEFAULT_MEASUREMENT_TUNABLES` the values. A focused test or an alternate run
copies the value and overrides fields:

- `sample_interval`: the sampling cadence, 100 ms
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
renderer (see `reference/output.md`).

## Hotpath reports

The script-bench runner gives every launch a hotpath report name of its own,
declares it to the launch's reservation where there is one, and removes it
after the launch.

## Control socket and guards

Every fresh hold binds `~/.brokkr/lock/ACQUISITION_ID.sock` in
`src/lock_service.rs`, which serves `stop` and status requests. Every
tracked-child window installs a `SigtermGuard` from `src/shutdown.rs`; guards
nest, and the outermost installs and restores the handlers. `src/oom.rs`
writes `1000` to the child's `oom_score_adj` before exec, ignoring errors.
