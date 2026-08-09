# brokkr results

Query the results database, `.brokkr/results.db` - the durable record every
measured run writes. The sibling command `brokkr sidecar` queries the profiler
store (`sidecar.db`); see `docs/commands/measure.md` for what lands in which.

Uniform across all projects, piners included (for its hotpath/alloc runs). The
exception is litehtml, whose only use of `results.db` is the unrelated
`MechanicalDb` schema - so `results`, `sidecar` and `invalidate` are hidden
from its `--help`.

## The table

Bare `brokkr results` prints the last `-n` rows (default 20), newest first. No
database yet reads as `no results yet (run a benchmark first)` rather than an
error - not having measured anything is a normal state.

`--top N` caps the functions shown in hotpath reports (default 10, `0` = all).

## Filters

All narrow the table, and all AND together:

- `--commit <REF>` - prefix match on the recorded commit.
- `--command <SUB>` - substring, so `read` matches `bench read`.
- `--mode <SUB>` - substring; the exact values are `bench`, `hotpath`,
  `alloc`. `--variant` is accepted as a legacy alias.
- `--dataset <SUB>` - substring against the input filename, so `eu` matches
  `europe-20260301-seq4714-with-indexdata.osm`.
- `--meta KEY=VALUE` - exact match on recorded metadata, key written without
  its `meta.` prefix. Repeatable.
- `--env NAME=VALUE` - exact match on a captured env var, name written without
  its `env.` prefix. Repeatable.

`--meta` and `--env` **exclude rows missing the key entirely**. For an A/B
where one arm is defined by an unset variable, record an explicit baseline
value (`=0`) on the off runs rather than relying on absence - or use
`--grep-v`, below, which can express absence.

## `--grep` / `--grep-v`

Substring match against the run's **invocation**, which spans three sources at
once: the subprocess argv (`cli_args`), the brokkr argv (`brokkr_args`), and
each captured env var rendered as `NAME=VALUE`.

That third source is the point. An arm gated by an environment variable appears
in no argv at all, so `--grep LAYER_STATS` is the only way to select it.

Both are repeatable, `git log --grep` style: every `--grep` must match (AND),
and any `--grep-v` hit excludes the row. They compose, which is what makes the
awkward A/B case expressible:

```
brokkr results --grep apply-changes --grep-v uring
```

That selects the arm distinguished only by an **absent** flag - something
`--grep` alone cannot say, and no `--env` filter can either. Both apply to
`--compare` exactly as they do here, so a comparison can be narrowed to one
arm.

## A single run

`brokkr results <uuid-prefix>` resolves a prefix to one row and prints a
labelled block instead of a table row: full multi-line `cli_args`, the brokkr
invocation, and the sidecar hint folded in as a field. For a `--bench N` row it
adds the per-iteration walls, and the `prev.*` provenance of what ran
immediately before it - the neighbouring run is often the explanation for an
outlier.

A prefix matching several rows falls back to the table plus per-row details.

A UUID with sidecar data but no results row reports itself as a **sidecar-only
run** (a dirty tree, or a run that failed before it could file a result) and
points at `brokkr sidecar`. Sidecar latest-keys such as `dirty` resolve to the
real UUID first, so an alias works wherever a prefix does.

## `--compare A B`

```
brokkr results --compare <COMMIT_A> <COMMIT_B>
```

Prints the two commits side by side, one delta row per matched pair. Two rows
pair when

```
(command, mode, input_file, brokkr_args, env_fingerprint)
```

agree. `brokkr_args` is in the key deliberately: arm-defining flags like
`--direct-io` must never collapse two different measurements into one averaged
row.

But two tokens are **stripped before the comparison** (`normalize_brokkr_args`
in `src/db/format/compare.rs`):

- `--commit REF` (and `--commit=REF`) - because `--commit` *is* the comparison
  axis. Keying on it would stop a retroactive row from ever pairing with the
  current-tree row it exists to be compared against, which is the entire
  purpose of recording it.
- `--verbose` / `-v` - noise that changes no measurement.

Pairs whose **host conditions differed** (memory, governor, kernel) are
annotated rather than silently rendered as a clean delta: the same numbers mean
something different across a governor change, and a comparison that hides that
is worse than no comparison. Differing **captured env** is annotated the same
way.

### `counters:`

Pairs whose stderr counters moved get one further line:

```
    counters: cells_evaluated 5000 -> 4000, prints 1240 -> 1180
```

A wall on its own cannot distinguish "the code got faster" from "the code did
less". This is what turns "12% faster" into "12% faster on 8% fewer cells". A
counter present on one side only is reported the same way - that is
instrumentation appearing or vanishing mid-series.

It is reported, never fatal. A brief predecessor let a workload declare which
counters were identity-bearing and made a move in one of them an error; the
declaration only controlled fatality, and a gate that fires on the first
legitimate win - doing less work is what most optimization actually is - earns a
bypass flag and then gets passed out of habit. Where a moved count really does
invalidate a series, that is the project's rule to enforce unconditionally, not
a per-comparison verdict.

`meta.`, `env.` and `prev.` pairs are excluded as provenance: they differ
between two runs without the work differing, and `prev.*` describes the
preceding run so it differs on nearly every pair.
