# pbfhogg vs elivagar: command plans and measurement contracts

Both projects supply plans to `src/measure/runner.rs`. One
`MeasurementSession` owns the lock, host paths, optional build and invocation
provenance. Their differences describe the commands being measured, rather
than separate dispatch loops. See `docs/commands/output-channels.md` for the
output channels.

## At a glance

| Aspect | pbfhogg | elivagar |
|---|---|---|
| Command plan | `PbfhoggPlan` | `ElivagarPlan` |
| Per-command params | `CommandParams` (I/O, snapshot, bbox) | typed built command or external tool |
| Build kinds | `pbfhogg-cli` main binary | main binary, cargo example, external tool |
| Bench timing | best-of-N external wall | external wall; examples self-iterate |
| I/O flags | `--direct-io`, `--io-uring`, `--compression` | none |
| Output promotion | snapshot graph | durable PMTiles archive store |
| External baselines | Java benchmarks in suites | Planetiler and Tilemaker |

## 1. Dispatch entry point and parameters

Callers invoke `measure::runner::run(req, &plan)`. Pbfhogg plans carry the
command, OSC sequence and `CommandParams`, including I/O flags, snapshot
selection, bbox and merged-cache policy. Elivagar plans carry the typed
`ElivagarCommand`, including its tilegen contract or example parameters.
Project-specific command modules construct those plans; the shared runner
owns measurement setup and execution after planning.

The runner checks project and profiling capabilities, returns after dry-run
planning, creates the session and executes the selected plan kind. Pbfhogg's
snapshot collision and I/O preflights still precede its build.

## 2. Build model

Pbfhogg uses the `pbfhogg-cli` release binary. Profiling adds its instrumentation
feature; the I/O selection adds its required cargo features.

Elivagar's enum selects its build shape:

- `Built(MainBinary)` builds tilegen.
- `Built(Example)` builds `bench_pmtiles` or `bench_node_store`.
- `External` materializes Java/Planetiler or C++/Tilemaker without a Rust build.

All builds pass through the same session. Tool setup and Planetiler's initial
source priming run outside timed samples.

## 3. Bench harness path and timing

Pbfhogg and tilegen use `run_external_prepared`: best-of-N external wall-clock
with microsecond resolution. The preparation callback resets sample output
before every execution. Stderr counters describe the winning sample but do
not replace its measured wall time.

Elivagar examples self-iterate with `--runs N`; brokkr executes one aggregate
sample through `run_internal`. Their existing `tiles`/`nodes_millions` and
`internal_runs` observations remain. Examples retain absent `cli_args`.
Planetiler and Tilemaker use the external wall-clock path with their existing
argv and Java/CMake profile labels.

## 4. Output channels into the DBs

External wall-clock and profiling runs capture the sidecar's `/proc` trajectory
and FIFO markers/counters into `sidecar.db`. External timing also accepts stderr
counters into the winning result's KV pairs. A self-reported `elapsed_ms` never
replaces external timing.

Elivagar examples' ordinary captured execution has no FIFO sidecar. Profiling
uses the shared hotpath capture path. Run mode persists neither result nor
sidecar records.

## 5. I/O mode flags

Pbfhogg supports `--direct-io`, `--io-uring` and `--compression`. I/O support is
checked per command, and io_uring has its preflight. The `write` and `merge`
benches refuse all three in favour of their own matrix flags. The flags retain their
literal spelling in recorded subprocess `cli_args`.

Elivagar has no corresponding I/O selection flags.

## 6. Output artifact lifecycle

The runner calls completion hooks only after all samples succeed. A process,
capture or promotion failure retains the current output for diagnosis.

Pbfhogg completion can promote a PBF into the snapshot graph before scratch
cleanup. Tilegen completion delegates to `elivagar/artifacts.rs`, which admits
the PMTiles output into `elivagar/archive_store.rs`. The store owns names, protected
directory checks and per-dataset/variant retention. Profiling tilegen cleans
its output after success without admitting an archive.

Shared inputs, merged caches, downloaded sources and tilegen checkpoints are
not sample scratch. `--skip-to` continues to use elivagar's checkpoint contract.

## 7. Run-specific metadata

Command names and subprocess `cli_args` retain their previous row identity.
Pbfhogg carries resolved input and cache observations. Tilegen profiling
continues to observe `meta.locations_on_ways_detected` from captured stderr.
The measurement mode and brokkr invocation come from the session; `BenchConfig`
has no fallback fields for either.

## What they share

The runner owns dry-run, mode selection, no-recording run mode, sample scratch
reset and the success boundary. Both projects use the same session, results and
sidecar stores, lock and worktree lifecycle.

Dry-run constructs argv without building, installing tools, starting a child
or creating run artifacts. Input verification may refresh the digest cache.
Elivagar's argv construction creates no directories.

## Reconciliation history

Tilegen moved from self-reported stderr timing to external wall-clock timing.
The remaining dispatch loops then moved to the shared command-plan runner,
including external tools, cargo examples and nidhogg's HTTP/server lifecycles.
