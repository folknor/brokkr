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
brokkr dellingr --lua same_obj_read,method --bench
brokkr dellingr --lua same_obj_read --lua method --hotpath
brokkr dellingr --all --bench
```

`--lua` selects one or more workloads by exact name, repeated or
comma-separated; `--all` selects every registered workload. Under `--hotpath`
/ `--alloc`, `--all` skips the workloads without a hotpath variant and warns
naming them, while a workload named with `--lua` refuses the run (see below).
Every
selected workload is resolved and its digest verified before anything builds,
so one typo'd name or drifted file refuses the whole run up front. The harness
builds once, then the workloads run in the order given, each filed as its own
row; a workload that fails does not skip the rest, and the run ends naming
every one that failed. A name given twice runs once.

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

Each pin is a parsed XXH128 file digest: exactly 32 hexadecimal characters.
SHA-256 values are rejected when brokkr loads the config.

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
a way for them to disagree with `measure::ProfiledMode::feature`.

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
verifies the digest (xxh128, mtime-cached via `preflight::verify_file_digest`)
before building anything, and refuses on mismatch:

```
[error]   preflight: hash mismatch for examples/fields/same_obj_read.lua
            expected: eaa3cf86979bad6b7d091923e9ddb132
            actual:   7b1d0c9f42ae86530fd1cc2b19e4a7f0
            origin: [dellingr.workloads.same_obj_read].hotpath_file in brokkr.toml
```

The `origin` line names the *pin*, not just the table: a workload carries two
of them, and only the one that drifted needs re-registering.

Retuning a workload is then a deliberate re-registration: edit the file, put
the new digest in `brokkr.toml`, and know that rows on either side of that
commit are not comparable. `brokkr results --compare` enforces it: the digest
is part of each row's `input_file` (see "Reading the results"), so rows of two
generations of a workload never pair.

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

For each workload, brokkr resolves `--lua <name>` and passes the path **as
registered** - relative to the directory holding `brokkr.toml` - as the
harness's single argument, with
that directory as the working directory (under `--commit` too: only the binary
comes from the worktree):

```
target/release/examples/<example> bench/same_obj_read.lua
```

The harness uses the string it is handed as the script's chunk name and its
`target=` value, so error locations read `bench/same_obj_read.lua:12:` and the
stored value is the same in every checkout and on every host. The absolute path
is brokkr's alone: it is what the existence and digest checks read.

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
uuid      timestamp            commit   command        mode     elapsed  dataset                 args
9f2a1c04  2026-07-25 11:41:02  4c19ab2  same_obj_read  bench    1240 ms  same_obj_read@eaa3cf86  --lua same_obj_read --bench 5
77b4f2ad  2026-07-25 11:20:44  8e0d113  same_obj_read  bench    1310 ms  same_obj_read@eaa3cf86  --lua same_obj_read --bench 5 --commit 8e0d113
```

One row series per workload is what makes the query surface useful:

- `brokkr results --command same_obj_read` - one workload across commits
- `brokkr results --compare <a> <b>` - pairs per workload automatically (the
  pair key is the tuple `brokkr man results` gives under `--compare`, the
  workload's `input_file` among it)
- `brokkr results --command same_obj_read --mode bench` - keeps instrumented
  runs out of a wall-clock series
- `brokkr results <uuid>` - per-iteration walls and `prev.*` provenance, and
  for a `--hotpath`/`--alloc` row the per-function table (calls, percent of
  total; `--top N` widens it). A stored run does not print that table itself
  (`-v` does not change this); an unstored one (`--force` on a dirty tree)
  prints it after its `[result]` line
- `brokkr sidecar <uuid>` - the phase markers and counters

A row's `input_file` is the workload file that ran and its verified digest,
`bench/same_obj_read.lua@<32-hex xxh128>`. That makes both part of the pair key:
the bench and hotpath variants are different files, and a re-registered
workload is a new generation, so neither pairs with rows of the other. The
`dataset` column shows the file's short name and a digest prefix, lengthened
past eight characters only when two generations shown together share it. Rows
recorded before workloads carried an `input_file` have an empty one and pair
only with each other.

## Scratch and clean

Dellingr has no project-specific scratch tree. Hotpath capture temporarily uses
`.brokkr/hotpath-report.json`, and sidecar measurement temporarily uses
`.brokkr/.sidecar-<token>.fifo`; both are removed after capture, and `brokkr
clean` removes a FIFO a killed brokkr left (`brokkr man clean`). `brokkr clean`
removes a legacy `.brokkr/dellingr/` directory if one remains. `results.db` and
`sidecar.db` stay in `.brokkr/` and are out of scope for `clean`.

## Out of scope

Comparison against reference Lua interpreters (lua5.2/5.4/5.5, luajit) is
**not** part of this surface - `scripts/bench.sh` in the dellingr repo owns
that, and `results.db` has no notion of a reference-implementation mean. The
shipped `dellingr` CLI binary is not benchmarked either; the example harness
is the whole bench surface.
