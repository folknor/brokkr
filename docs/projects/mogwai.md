# brokkr mogwai

`brokkr mogwai` benchmarks a mogwai surface. There are two kinds of surface,
one row shape and no layers. This page covers the surfaces, the target
registry, how rows pair, how `--commit` picks a working directory, and the
counters `--compare` reports. These invocations list both kinds, bench a CLI
surface and bench a harness surface:

```sh
brokkr mogwai
brokkr mogwai -- gen --type summary --seed 1
brokkr mogwai screen_projection --hotpath
```

## Surfaces

mogwai's surfaces come in two kinds:

- **Argv-shaped**: run through the shipped bin. These are `gen` and its
  `--type` variants, `tick-composition`, `preflight`, `measure`, `fit`,
  `cache`, `synth` and `arrival-screen`: a process, an argv, a wall. Benching
  them through `target/release/mogwai` measures what ships, startup and
  argument parsing included, which is the honest end-to-end number.
- **Harness-shaped**: run through a cargo example. These are the engine's
  matching loop and divergence seam, the `TickSource` implementations, the
  arrival draw and the screen's projection. They have no command line, so
  the harness is the addressable thing.

## Registry

The `[mogwai]` table names the CLI package and bin, and each
`[mogwai.targets.NAME]` table registers one harness target and its feature
shape. The registry holds nothing else:

```toml
[mogwai]
package = "mogwai-cli"     # cargo -p for the CLI
bin     = "mogwai"         # optional, defaults to the package name

[mogwai.targets.screen_projection]
package  = "mogwai-lab"    # optional, defaults to [mogwai] package
example  = "screen_projection_bench"
features = ["hotpath"]
```

The registry follows these rules:

- **CLI surfaces need no registration.** The bin is registered once, and you
  compose the argv at the call site.
- **Harness surfaces resolve by name** against `[mogwai.targets.*]`. Adding
  one to the measurable set means registering a target, work you'd do the
  moment you wanted to optimize it.
- **Harnesses take an argv**, like the bin, so one target covers every
  preset, window, seed and cell you pass it.
- **`--bench`, `--hotpath` and `--alloc` apply to both kinds.** There's one
  row shape and one query surface. The instrumented modes point the child
  at a report file and read it back; `--bench` measures an external wall.

### features

The `features` key lists the cargo features a target is built with.
`--hotpath` and `--alloc` record nothing without the feature that compiles
the instrumentation in, so a target you must remember to build with
`--features hotpath` records profile-less rows.

brokkr builds the union of the registered list and what the mode and the
call site add:

- `--hotpath` adds `hotpath`.
- `--alloc` adds `hotpath-alloc`, since alloc alone tracks nothing.
- A call-site `-F` adds an arm.
- A feature already registered isn't repeated.

The registered list is never dropped, because it's often what makes the
target build at all (an instrumented example's `required-features`). So if
a target registers `hotpath`, its `--bench` walls are measured on an
instrumented build. Register the feature only on the targets that need it.

## No workload registry

brokkr registers targets, never named workloads: you compose every
invocation at the call site, and brokkr records it verbatim. Nothing stops
you invoking a harness with an argv in a shape nobody meant; the captured
argv in the row is what tells you two runs aren't comparable.

A per-entry workload field from an older mogwai registry has a call-site
replacement:

| Field | Use instead |
|---|---|
| `timing`, `timing_reason` | Markers plus `brokkr sidecar --durations` |
| `expect_seconds` | The stored history of earlier rows |
| `runs` | `--bench N` at the call site |
| `identity_counters` | The `--compare` counter line; see "Counters" below |
| `{corpus}` and its token | Datasets |
| `successor` | Pairing on the captured argv |
| `args` | The argv after the target name |

## Rows and pairing

Rows file under the target name, or the bin name for a CLI invocation.
brokkr records the subprocess invocation in `cli_args` and its own in
`brokkr_args`. New rows pair on the tuple `brokkr man results` gives under
`--compare`. Its `arm_key` keeps measurement-affecting options and excludes
brokkr's own commit, verbosity and iteration-count provenance.

brokkr keeps the forwarded argv verbatim. A surface's own `-v`, `--commit`
or `--bench N` is part of the arm, whether or not a `--` precedes it.
`brokkr man results` describes how historical rows pair.

Selecting an arm is therefore a query. `brokkr results --grep` and
`--grep-v` match the whole invocation, which is the only way to select an
arm defined by an absent flag.

### Inputs

brokkr leaves `input_file`, `dataset` and `variant` empty, because it
doesn't know what a surface reads:

- An input the argv names is already in the row, as part of the captured
  invocation.
- An input the surface defaults internally is named nowhere in the row.
  `measure`, for one, reads its corpus, jobs manifest and preflight from
  repo-relative defaults.

No row identifies the contents of what was read. `brokkr env` checks a
`[HOST.datasets.*]` entry in mogwai's `brokkr.toml`, but `brokkr mogwai`
never reads one.

## Commit builds

`--commit REF` builds the surface in a worktree of `REF`, as every measured
command does. It runs the surface with the live checkout (the project root)
as its working directory, the split `brokkr man run` describes for
`run --commit`. Both arms of an A/B resolve run-time relative paths against
the same directory. Their inputs can still differ: a default can change
between the two builds, and so can what it points at.

The worktree would be the wrong working directory. mogwai's market-data
corpus under `research/` and its cache under `analysis/out/` are gitignored,
so a `REF` checkout doesn't populate them. The `--commit` arm would then
read a different tree from the arm it's compared against.

The split has these consequences:

- **Relative inputs are the live checkout's copies**, including defaults
  inside mogwai. A tracked input may be dirty or newer than `REF`, so old
  code reads today's format, and fails or misreads where the format moved.
- **Paths rooted on `CARGO_MANIFEST_DIR` point into the worktree**, since
  the build bakes them in. `tape_lateness`'s default config is one: under
  `--commit` it's `REF`'s copy whatever the working directory.
- **Relative outputs are written to the live checkout.** A successful
  `measure` with its default `--out` rewrites the tracked
  `analysis/mnq-measure-12a.json`. If the content changed, the tree is dirty
  for the next iteration and the other arm, and mogwai's own clean-tree gate
  can then refuse them.
- **A surface that asks git about its working directory sees the live
  checkout.** mogwai's tree-attested artifact writers, `measure` and `fit`
  among them, bind `git rev-parse HEAD` of their working directory as the
  commit that produced the artifact. Under `--commit` they record the live
  HEAD rather than `REF`. That's a mogwai defect, not this split's contract:
  a stale binary run in a checkout that has moved on misbinds the same way
  without brokkr.
- **The row files under `REF` even when the live checkout is dirty**,
  since the dirty-tree refusal checks the build root (`brokkr man measure`).
  Nothing in the row says which state of the live checkout the inputs came
  from. That's part of a wider gap: `--commit` rows don't identify what
  besides the code shaped the run.

## Counters

Every external run scrapes the winning run's stderr for `key=value`
counters, at no cost because stderr is captured either way. `--compare`
reports the counters that moved:

```text
    counters: cells_evaluated 5000 -> 4000, prints 1240 -> 1180
```

A wall alone can't tell "the code got faster" from "the code did less".
Counters turn "12% faster" into "12% faster on 8% fewer cells".

A moved counter is reported, never fatal, since doing less work is what
most optimization is. Where a moved count does invalidate a series, such as
a seeded tape whose draw moved, bump mogwai's `TAPE_PROTOCOL_VERSION`, which
is unconditional and can't be waived per comparison.

brokkr reads counters from the winning run only: averaging across
iterations would hide an iteration that differed rather than surface it.
Only unprefixed counters count. `meta.`, `env.`, `prev.` and brokkr's other
namespaced pairs are provenance and excluded.

## Scope

`brokkr mogwai` doesn't measure mogwai's serving path or adapter.

mogwai's reading rules, what makes a delta real, live in mogwai's own
performance reference, not here. pbfhogg's rules are almost entirely
inapplicable: pbfhogg is I/O bound, so its error model is drive state, trim
debt and page cache, while the screen and the walk are CPU and RNG bound.
