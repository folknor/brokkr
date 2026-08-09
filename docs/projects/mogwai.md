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

Workloads are timed EXTERNALLY - brokkr's wall-clock around the child, best of
N - not from a self-reported `elapsed_ms` on stderr.

These are whole CLI invocations whose setup cost is part of what is being
measured. External timing also means a baseline can be recorded at any commit
whose CLI still parses the frozen argv, including retroactively via `--commit`,
because `--compare` strips `--commit` from the pairing key. Nothing has to land
inside mogwai before the first row exists.

A workload that one day needs its own measured window - excluding, say, an
expensive corpus load - is the case for a self-reported wall, and it should say
so in its registration rather than change this default underneath the rest.

## What a row looks like, and why two columns are empty

Rows are filed under the workload name, following dellingr. Two fields are
deliberately empty rather than merely unused:

- `input_file`, because a generated workload's inputs are part of its
  definition. It is seeded, not read; duplicating the name into that column
  would only widen the table.
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

> [!NOTE]
> The field is validated and stored today, but nothing enforces it yet -
> capturing counters requires a harness path that keeps external timing while
> scraping stderr `key=value`, which does not exist yet (`run_external` ignores
> stderr; `run_external_with_kv` switches to self-reported timing). Until both
> land, `identity_counters` is a declaration, not a gate.

## Not covered yet

- **Corpus workloads.** Workloads reading delivered archives (`measure12a`,
  `fit`, `preflight`, `characterize-pair`) need a per-host, hash-pinned path
  registry. Only generated workloads - self-contained given preset, window and
  seeds - are supported today.
- **Work-size counters**, per the note above.
- **The serving path** (sockets, pacing) is excluded by design, not omission. It
  is a different measurement class, wall-clock- and environment-sensitive, and
  it does not gate the offline work.
