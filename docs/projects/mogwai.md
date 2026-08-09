# brokkr mogwai

The layer-1 regression bench: the durable numbers, tracked across months, one
row series per named workload, paired with the commit that produced them.

Mogwai is a CLI, a pipeline and a library of building blocks at once, so its
benchmarking borrows from three siblings - elivagar's config-owned workload
contract, sluggrs' harness mechanics, pbfhogg's sidecar channel. This command
is the first of those: the durable layer. The optimization instruments that
churn during a round (harness examples, `hotpath` annotations) are a separate
layer and deliberately not managed here.

## The unit is a frozen invocation

Not a subcommand mirror, and not a pinned file.

Mogwai's commands take config-shaped arguments - preset, window, seed set,
probe mode - so no single file's digest stands for "the work" the way
dellingr's `.lua` does, and a row filed under a bare subcommand name would say
nothing about which arguments produced it. The registry pins the argv instead,
stated in full: nothing defaulted, nothing auto-detected.

That rule is elivagar's, and it is here for elivagar's reason. Auto-detection
once let two runs of the same binary on the same input do different work with
nothing in the invocation saying which.

A workload NAME is therefore a promise: that its rows are comparable across
months. Changing an invocation is a new name, never a quiet edit.

## Configuration

```toml
[mogwai]
package = "mogwai-cli"     # cargo -p; the crate carrying the CLI
bin = "mogwai"             # optional, defaults to the package name

[mogwai.workloads.screen-probe]
description = "coarse+refine screen over the standard probe window"
args = ["screen", "--preset", "standard", "--probe", "--seed", "7"]
runs = 3
expect_seconds = 20
identity_counters = ["parents", "prints"]
```

| Field | Meaning |
|---|---|
| `args` | The complete argv, stated in full. Required, non-empty. |
| `description` | What the workload measures; shown by the bare index. |
| `runs` | Repeat count for this workload; falls back to the global default. |
| `expect_seconds` | Rough duration. Never enforced - it makes the cost of a baseline refresh legible before it is paid. |
| `identity_counters` | Work-size counters that must not move between compared rows. |
| `timing` | `external` (default) or `self_reported`. Which clock the row's `elapsed_ms` comes from. |
| `timing_reason` | Why. Required for `self_reported`, optional otherwise. |
| `corpus` | Names a `[<host>.corpus.*]` archive; absent means a generated workload. |
| `successor` | Names the workload that replaced this one. Present means retired. |

Measurement flags (`--bench`, `--hotpath`, `--alloc`) are rejected inside
`args`. Mode is brokkr's axis, independent of the target; baking one into the
contract would make a single name mean two different measurements.

## Usage

```
brokkr mogwai                      # list the registry (bare-is-an-index)
brokkr mogwai screen-probe         # run once, store nothing
brokkr mogwai screen-probe --bench # record a row
```

## Timing

Timing is a PER-WORKLOAD declaration, because which clock is honest is a
property of the workload rather than of the runner. Both modes scrape stderr
`key=value` counters; they differ only in where the recorded `elapsed_ms` comes
from.

**`timing = "external"`** (the default) takes brokkr's wall-clock around the
child, best of N (`run_external_with_counters`; see
`brokkr man output-channels`). An `elapsed_ms` on stderr is optional here and,
if present, is kept as an ordinary counter rather than replacing the measured
wall, so the two numbers stay independently visible.

External is the default for a reason worth stating plainly: it needs NOTHING
inside mogwai. A baseline can be recorded at any commit whose CLI still parses
the frozen argv, including retroactively via `--commit`, because `--compare`
strips `--commit` from the pairing key. History exists before the
instrumentation does.

**`timing = "self_reported"`** takes the target's own `elapsed_ms=` and
discards brokkr's wall (`run_external_with_kv`). For a workload with real setup
that is not the code under test - a multi-gigabyte corpus verified and a cache
loaded before any measured work - where an external wall would fold that
constant cost into every reading of the thing being optimized. A missing
`elapsed_ms=` is a hard error in this mode.

The cost is that history reaches back only to the commit that first emitted the
line; there is nothing to back-fill from. So `timing_reason` is REQUIRED here
and the config is rejected without it. The reason is the part that gets
forgotten, and a narrowed window is exactly the choice someone reverses a year
later because the entry never said what it excluded or why that was legitimate.

> [!WARNING]
> Switching a live workload's `timing` changes what its number MEANS, so it is
> subject to the same new-name rule as `args`. Rows record their clock as
> `meta.timing`, and `--compare` refuses to read a delta across a switch:
>
> ```
>     TIMING CHANGED: external -> self_reported - the two walls measure
>     different windows and their delta is not a speedup
> ```
>
> One side lacking the key is not a disagreement: rows predating `meta.timing`
> are all external, which is also the default.

## What a row looks like, and why two columns are empty

Rows are filed under the workload name, following dellingr. Two fields are
deliberately empty rather than merely unused:

- `input_file`, for a GENERATED workload, because its inputs are part of its
  definition. It is seeded, not read; duplicating the name into that column
  would only widen the table. A corpus workload does fill it - with the registry
  key, per below.
- `brokkr_args`, because it is a `pair_key` component. The expanded argv is
  recorded in `cli_args`, where `brokkr results` and `--grep` still see it, but
  keeping it out of the key is what lets a workload's rows keep pairing across
  a re-registration that did not change the work.

The guard against a registration that DID change the work is the new-name rule,
enforced at resolution - not the pairing key.

## Retirement and lineage

When an invocation must change, register a new name and point the old entry at
it:

```toml
[mogwai.workloads.screen-probe-v1]
args = ["screen", "--preset", "standard", "--probe"]
successor = "screen-probe"
```

A retired workload refuses to run, and the refusal names its heir. That refusal
is the point: the alternative is a name that still works but no longer measures
what its historical rows measured.

The old rows stay queryable (`brokkr results --command screen-probe-v1`). The
pointer is what lets a reader crossing the rename find the new series instead of
concluding the history was deleted. A `successor` naming an unregistered
workload is rejected at parse time - a lineage pointer that leads nowhere is
worse than none, because it claims the history is reachable and it is not.

## Determinism, and what it is for

Every benched workload is seeded, so the work is bit-identical run to run and
across both sides of an A/B at the same seed. Variance is scheduler noise only,
and a wall move outside noise is a finding rather than a shrug.

`identity_counters` is the declared set of work-size counters whose movement
invalidates a comparison - the classic optimization failure of accidentally
doing less work and calling it a speedup. It is a declared SET rather than
"every counter must match" on purpose: counters like `cells_evaluated` are
exactly what a real optimization is supposed to move, and a blanket rule would
fire on the first legitimate win, earn a bypass flag, and then be passed
habitually until it meant nothing.

When a declared counter moves between two compared rows, `--compare` annotates
the pair:

```
    WORK CHANGED: parents 1240 -> 1180 - the wall delta above is not a speedup
```

A counter present on one side and absent on the other is reported the same way -
that is what instrumentation appearing or vanishing mid-series looks like, and
it makes the pair no more comparable than a changed value does. A counter absent
from *both* sides is silent: the declaration may simply be ahead of the
instrumentation that will emit it.

The UNDECLARED counters that moved are reported too, on their own non-fatal
line:

```
    counters: cells_evaluated 5000 -> 4000, sessions 23 -> 23
```

That is the other half of the declared/undeclared split, and it is what turns
"12% faster" into "12% faster on 8% fewer cells". Recorded and diffed, never
fatal - a counter the workload did not declare is one it expects a real
optimization to move.

Both lines are emitted only for rows that opted into the counter contract, which
means rows carrying `meta.identity_counters` - written by `brokkr mogwai` on
every row, even when the declared set is empty. Every project puts numbers in
its kv; only a declared workload asked for them to be read as a statement about
whether two runs did the same work. Reading an elivagar row that way would be
inventing a claim it never made.

The declaration travels WITH each row (`meta.identity_counters`), not read from
config at comparison time. Re-declaring a workload's set must not retroactively
change what a comparison between two older rows asserted - those rows were
produced under the old declaration, and that is the one their delta was judged
against.

Counters reach the row from the winning run's stderr `key=value` lines. Only the
winner's are kept: the work is seeded and therefore identical every iteration,
so averaging across runs would hide an iteration that somehow differed rather
than surface it.

## Corpus workloads

A workload that reads a delivered archive names it, and marks where the path
belongs with `{corpus}`:

```toml
[mogwai.workloads.measure12a]
args = ["measure", "--input", "{corpus}", "--protocol", "12a"]
corpus = "july-delivery"
runs = 1
expect_seconds = 900

[frigg.corpus.july-delivery]
path = "research/market-data/july.parquet"
xxh128 = "..."
```

The registry is per host because these are multi-gigabyte deliveries that live
wherever the machine holding them put them. The NAME is not per host, which is
what keeps a corpus row comparable across machines: rows file their `input_file`
under the registry key, never the resolved path. That column is a `pair_key`
component, so filing the path would make the same measurement on two machines
look like two different benchmarks - while filing the key correctly keeps two
runs over *different* corpora from pairing.

The token exists so "stated in full" survives a per-host path: the invocation
still says exactly which input it takes, naming it by registry key rather than
by a path that means nothing on another machine. The two halves are checked
against each other at parse time - a `corpus` whose args never place it is
rejected, and so is a `{corpus}` token with nothing to fill it.

Relative paths resolve against the directory holding `brokkr.toml`; absolute
paths are allowed, because a delivery commonly sits outside the repository.

> [!IMPORTANT]
> The digest is **XXH128**, brokkr's standard file hash - not the SHA-256 the
> delivery manifests carry. Transcribing a manifest is therefore not enough:
> register the digest brokkr reports. A second hash implementation would buy
> nothing against what this defends (drift, not tampering) and would forfeit the
> mtime cache that keeps a multi-gigabyte archive from being re-read every run.

`xxh128` is therefore OPTIONAL, and `brokkr env` is where the value comes from.
Register the path, run `brokkr env`, and the entry appears under `corpus:` with
the digest computed from the file on disk:

```
corpus:      july-delivery <tick> (no hash configured, actual: 3f2a...)
```

Paste that back into the entry. A workload over an unregistered digest still
runs - refusing would leave no way to reach the first run - but warns that it
is UNVERIFIED on every run. A digest that is registered and does not match
refuses, as before. This is exactly how `[<host>.datasets.*]` already behaves,
and the two registries print in the same shape for that reason.

Generated workloads never consult the host registry, so a machine holding no
deliveries can still run every generated workload without registering anything.

## Not covered

**The serving path** (sockets, pacing) is excluded by design, not omission. It
is a different measurement class - wall-clock- and environment-sensitive - and
it does not gate the offline work.
