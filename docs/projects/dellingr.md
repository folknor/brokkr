# dellingr

`project = "dellingr"` in `brokkr.toml`. Single crate: a pure-Rust Lua VM with
precise instruction-cost accounting.

One command, `brokkr dellingr`, plus the shared surface (`check`, `test`,
`clippy`, `results`, `sidecar`, `clean`, ...).

```
brokkr dellingr --lua same_obj_read --bench
brokkr dellingr --lua same_obj_read --hotpath
brokkr dellingr --lua same_obj_read --alloc
brokkr dellingr --lua same_obj_read --bench --commit <ref>
```

## What makes this project's bench surface different

Almost every resolution step the map-data projects need is absent here.
Workloads are short `.lua` files tracked in git, so there are no external
datasets, no host data dirs, no scratch tree and no drive config. Runs are
single-threaded, CPU-bound, and do no I/O.

Four things *are* specific:

1. **The harness is a cargo example, not a bin.** `[dellingr] example` names
   the target.
2. **Workloads are hash-pinned.** They are editable source, not immutable
   input data.
3. **`--commit` deliberately mixes two trees.** The harness comes from the old
   worktree; the workload does not.
4. **Instrumented modes resolve a different file.** `--hotpath` / `--alloc`
   require the workload's `hotpath_file` / `hotpath_xxh128` pair and refuse
   without it - see below.

## Configuration

```toml
[dellingr]
example = "hotpath"

[dellingr.workloads.same_obj_read]
file = "bench/same_obj_read.lua"
xxh128 = "eaa3cf86979bad6b7d091923e9ddb132"
hotpath_file = "examples/fields/same_obj_read.lua"
hotpath_xxh128 = "..."
```

`example` is the cargo `--example` target implementing the bench harness.

**Features are not configured.** The measurement mode picks them, using
brokkr's standing convention across every project:

| mode        | build                                |
| ----------- | ------------------------------------ |
| `--bench`   | the example, bare                    |
| `--hotpath` | `--features hotpath`                 |
| `--alloc`   | `--features hotpath-alloc`           |

`--bench` is the mode whose walls are trusted, which is exactly why it builds
uninstrumented. Restating the two feature names per project would only create
a way for them to disagree with `harness::hotpath_feature`.

`file` and `hotpath_file` are relative to the directory holding `brokkr.toml`;
absolute paths are rejected at parse time, as are empty digests and a
half-registered hotpath pair. The registry is deliberately **not** hostname-keyed,
unlike `[<host>.datasets.*]` - workloads are in-repo files identical on every
host, so a per-host section would imply a variability that does not exist.

### Two files per workload: bench scale vs instrumentation scale

The mode families need opposite workload scales, so a workload registers two
files carrying the same kernel:

- `file` / `xxh128` - resolved by `--bench` and plain runs. Seconds-scale, so
  wall deltas resolve above launch-to-launch noise.
- `hotpath_file` / `hotpath_xxh128` - resolved by `--hotpath` / `--alloc`.
  Instrumentation-scale (tens of ms per `_bench` call).

The split is a hard requirement, not a tuning preference. The hotpath crate
queues one event per instrumented call in an unbounded per-thread queue; its
background consumer sweeps every 50ms with a per-queue drain cap, orders of
magnitude below what a VM dispatch loop produces. A seconds-scale workload
under instrumentation backlogs tens of GB of RAM and dies (or takes the host
down with it) before the report exists - observed at 30+ GB RAM + swap per
run on 2026-07-26. Instrumented runs of an ms-scale variant still peak at a
few GB of RSS from the same mechanism; that is expected and harmless.

Because the failure mode is that severe, the pair is **required**: an
instrumented run of a workload without `hotpath_file` refuses with the
rationale rather than falling back to `file`. A half-registered pair is a
parse-time config error, so it is named whether or not anyone runs that
workload. Both pins re-register independently - editing the bench file does
not invalidate stored hotpath rows, and vice versa.

Instrumented walls were already documented as untrustworthy (read
distributions, not times), so nothing is lost by the file swap: per-function
call counts and percentages are scale-invariant for a fixed kernel.

### Why the hash

A workload is editable code. Retuning one without re-registering it would
leave every stored row filed under that name describing a workload that no
longer exists - the history silently stops meaning what it says. brokkr
verifies the digest (xxh128, mtime-cached via `preflight::verify_file_hash`)
before building anything, and refuses on mismatch:

```
[error]   preflight: hash mismatch for /home/f/dellingr/examples/fields/same_obj_read.lua
            expected: eaa3cf86979bad6b7d091923e9ddb132
            actual:   7b1d0c9f42ae86530fd1cc2b19e4a7f0
            origin: [dellingr.workloads.same_obj_read].hotpath_file in brokkr.toml
```

The `origin` line names the *pin*, not just the table: a workload carries two
of them, and only the one that drifted needs re-registering.

Retuning a workload is then a deliberate re-registration: edit the file, put
the new digest in `brokkr.toml`, and know that rows on either side of that
commit are not comparable.

## `--commit` semantics

`--commit <ref>` builds the harness from that commit's worktree, as everywhere
else in brokkr. **The workload file does not come from the worktree** - it is
resolved from the registration in the current tree, and the hash check applies
to the file actually loaded.

A baseline exists to vary the VM while holding the workload fixed. The old
commit's copy of the same path may differ, and benchmarking *that* would
attribute a workload change to the VM. This is the one place brokkr's
`project_root` (the tree holding `brokkr.toml`) and `build_root` (the
worktree) must not be interchanged; see `src/dellingr/workload.rs`.

## The harness contract

brokkr resolves `--lua <name>` to a path and passes it as the harness's single
argument:

```
target/release/examples/<example> /abs/path/to/workload.lua
```

The harness speaks the standard sidecar protocol - nothing dellingr-specific:

- `BROKKR_MARKER_FIFO` is set on every mode. Emit `FOO_START` / `FOO_END`
  marker pairs for phases (parse, setup, warm, and batched warm-block
  sub-spans; tens of spans per run is well within range).
- `@name=value` counters (heap bytes, object counts, iteration counts) go down
  the same FIFO and land in `sidecar.db`.
- Under `--hotpath` / `--alloc`, `HOTPATH_OUTPUT_PATH` points at the JSON
  report brokkr collects.

Both paths are shared machinery: see `docs/commands/measure.md` for the marker
protocol and `docs/commands/output-channels.md` for which channel lands in
which store.

## Reading the results

Rows are filed under the **workload name**, not under `dellingr`:

```
uuid      timestamp            commit   command        mode     elapsed  dataset  args
9f2a1c04  2026-07-25 11:41:02  4c19ab2  same_obj_read  bench    1240 ms           --lua same_obj_read --bench 5
77b4f2ad  2026-07-25 11:20:44  8e0d113  same_obj_read  bench    1310 ms           --lua same_obj_read --bench 5 --commit 8e0d113
```

One row series per workload is what makes the query surface useful:

- `brokkr results --command same_obj_read` - one workload across commits
- `brokkr results --compare <a> <b>` - pairs per workload automatically (the
  pair key is `(command, mode, input_file)`)
- `brokkr results --command same_obj_read --mode bench` - keeps instrumented
  runs out of a wall-clock series
- `brokkr results <uuid>` - per-iteration walls and `prev.*` provenance
- `brokkr sidecar <uuid>` - the phase markers and counters

The `dataset` column is empty by design. It means "which external corpus", and
dellingr has none; the workload is the operation, already in `command`.

## Scratch and clean

The harness's hotpath report and marker FIFO live in `.brokkr/dellingr/`,
rather than the default `data/scratch` (which would create a `data/` tree in a
project that has no data). A routine `brokkr clean` removes that directory.
`results.db` and `sidecar.db` are one level up in `.brokkr/` and are out of
scope for `clean`, as in every project.

## Out of scope

Comparison against reference Lua interpreters (lua5.2/5.4/5.5, luajit) is
**not** part of this surface - `scripts/bench.sh` in the dellingr repo owns
that, and `results.db` has no notion of a reference-implementation mean. The
shipped `dellingr` CLI binary is not benchmarked either; the example harness
is the whole bench surface.
