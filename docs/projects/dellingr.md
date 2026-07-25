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
Workloads are 20-line `.lua` files tracked in git, so there are no external
datasets, no host data dirs, no scratch tree and no drive config. Runs are
single-threaded, CPU-bound, do no I/O, and target seconds-scale wall times.

Three things *are* specific:

1. **The harness is a cargo example, not a bin.** `[dellingr] example` names
   the target.
2. **Workloads are hash-pinned.** They are editable source, not immutable
   input data.
3. **`--commit` deliberately mixes two trees.** The harness comes from the old
   worktree; the workload does not.

## Configuration

```toml
[dellingr]
example = "hotpath"

[dellingr.workloads.same_obj_read]
file = "examples/fields/same_obj_read.lua"
xxh128 = "eaa3cf86979bad6b7d091923e9ddb132"

[dellingr.workloads.patterns]
file = "examples/strings/patterns.lua"
xxh128 = "..."
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

`file` is relative to the directory holding `brokkr.toml`; absolute paths are
rejected at parse time. The registry is deliberately **not** hostname-keyed,
unlike `[<host>.datasets.*]` - workloads are in-repo files identical on every
host, so a per-host section would imply a variability that does not exist.

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
            origin: [dellingr.workloads.same_obj_read] in brokkr.toml
```

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
