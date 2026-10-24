# brokkr results

Query the results database, `.brokkr/results.db` - the durable record every
measured run writes. The sibling command `brokkr sidecar` queries the profiler
store (`sidecar.db`); see `docs/commands/measure.md` for what lands in which.

`results` and `sidecar` open existing stores read-only and do not create
databases. A store on an older schema is migrated first (the same transactional
migrations a measured write applies, then the query proceeds read-only), so an
upgrade never makes existing history unreadable. A store stamped with a newer
schema version than this brokkr knows is refused, by queries and writes alike;
a stamp is never lowered.

Uniform across all projects, piners included (for its hotpath/alloc runs). The
exception is litehtml, whose only use of `results.db` is the unrelated
`MechanicalDb` schema - so `results`, `sidecar` and `invalidate` are hidden
from its `--help`.

## The table

Bare `brokkr results` prints the last `-n` rows (default 20), newest first. No
database yet reads as `no results yet (run a benchmark first)` rather than an
error - not having measured anything is a normal state.

`--limit N` caps rows in table and UUID-prefix views, and comparison pairs in
`--compare` (default 20). Compare pairs are formed before the cap is applied.
When a UUID-prefix view is capped, it prints how many matching result rows
were omitted so the cap is visible.

`--top N` caps the functions shown in hotpath reports (default 10, `0` = all).

## Precision

Every row has an integer `elapsed_ms`. Most rows also have `elapsed_us`, the
exact wall in microseconds: the external and hotpath paths time it
themselves, the stderr-kv path takes it from a fractional `elapsed_ms=` line,
and distribution runs (nidhogg `api`) record the fastest sample. Where
`elapsed_us` exists, `elapsed_ms` is that value rounded to nearest.

Every view - the table, a single run, `--compare` - prints the microsecond
reading when a row has one, as milliseconds with three decimals (`0.312 ms`,
`6.847 ms`), and the integer otherwise (`1234 ms`). So a sub-millisecond run
does not print as `0 ms`, and a view never disagrees with another about the
same row. Rows recorded before a path learned microseconds have no
`elapsed_us` and print as integers; that detail was never measured, so it is
never invented from `elapsed_ms * 1000`.

The same rule applies to each distribution statistic. Per-iteration walls keep
their precision individually: exact samples print with three decimals and
legacy samples print as integers. The list remains in execution order.

## Filters

All narrow the table, and all AND together:

- `--commit <HASH>` - prefix match on the recorded commit, in both
  directions: the recorded hash may start with what you typed, or what you
  typed may start with the recorded hash (4+ digits). Rows carry the width
  brokkr abbreviated to when they were recorded - it used to grow with the
  repo - so a full hash from `git log` still finds a row stored at 7. The same
  match selects each side of `--compare`. A stored commit that is not a hash
  at all (legacy text such as `unknown`) never makes a query fail: the row is
  shown with that text verbatim.
- `--command <SUB>` - substring, so `read` matches `bench read`.
- `--mode <SUB>` - substring; the exact values are `bench`, `hotpath`,
  `alloc`. `--variant` is accepted as a legacy alias.
- `--dataset <SUB>` - substring against the input filename, so `eu` matches
  `europe-20260301-seq4714-with-indexdata.osm`.
- `--meta KEY=VALUE` - exact match on recorded metadata, key written without
  its `meta.` prefix. Repeatable.
- `--env NAME=VALUE` - exact match on a captured env var, name written without
  its `env.` prefix. Repeatable.

Prefixes and substrings are literal: `_` and `%` match only themselves (they
used to be SQL `LIKE` wildcards), while ASCII case is still ignored. `--meta`
and `--env` compare the stored value as text whichever type it was recorded
as, so `--meta threads=8` matches an integer `8`; a real is SQLite's text form
of it (`2.0`, not `2`).

`--meta` and `--env` **exclude rows missing the key entirely**. For an A/B
where one arm is defined by an unset variable, record an explicit baseline
value (`=0`) on the off runs rather than relying on absence - or use
`--grep-v`, below, which can express absence.

Before persistence, brokkr redacts recognized credential-bearing argv values
and captured environment values, including URL credentials and sensitive URL
parameters. Only a credential name is redacted (`--token`, `API_KEY=`,
`--env PASSWORD=...`, an `Authorization` header): ordinary `--env` arms and
paths are kept, so pairing and `--grep` still see them. Existing JSON argv and captured environment entries are scrubbed
when the results store is upgraded. Older joined argv strings are kept as
recorded because their original argument boundaries cannot be recovered
reliably.

Global command-history argv uses the same credential redaction rule and also
hides the value of its `--project-dir` option. Rows recorded before the
credential rule was narrowed may already show `<redacted>` for ordinary
`--env` values; those values cannot be recovered, so the stored redaction
remains.

These row filters also narrow UUID lookups and apply to both sides of
`--compare`; the commit prefixes select the comparison axis.

## `--grep` / `--grep-v`

Substring match against the run's **invocation**, which spans three sources at
once: the subprocess argv (`cli_args`), the brokkr argv (`brokkr_args`), and
each captured env var rendered as `NAME=VALUE`. New `brokkr_args` values are
JSON arrays, preserving spaces and punctuation inside each argument; older
rows keep their original joined string.

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

When `brokkr results <uuid-prefix>` matches one result row, it prints a
labelled block instead of a table row: full multi-line `cli_args`, the brokkr
argv array, and the sidecar hint folded in as a field. For a `--bench N` row it
adds the per-iteration walls, and the `prev.*` provenance of what ran
immediately before it - the neighbouring run is often the explanation for an
outlier. A distribution row (nidhogg `api`) also gets its `samples`, `min`,
`p50`, `p95` and `max`.

If `--stop` was configured, the block shows the requested marker separately
from the marker that actually stopped the child. A request that never fired
does not say the child was killed.

Any supplied row filters still apply to the UUID matches.

A prefix matching several runs falls back to the table plus per-row details.
UUID resolution considers both stores; results lists result rows and reports
any matching sidecar-only UUIDs separately. `sidecar` requires a prefix to
name one run and lists candidates when it is ambiguous. An empty prefix is
refused rather than matching every row.

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
(command, mode, input_file, arm_key, env_fingerprint, cargo_profile)
```

agree. `arm_key` represents measurement-affecting options, so arm-defining
flags like `--direct-io` keep different measurements separate without
reparsing argv in the comparison code. `cargo_profile` is there because a
config-selected profile (`[ratatoskr.harness] profile`, say) changes the
binary without changing the argv. A row built under anything but release
gets a `build:` line naming its profile, so rows a profile split apart say
why. A row whose profile is missing or unreadable pairs with nothing, not
even with another row carrying the same text: nothing is known about how
either binary was built. Equal profiles are necessary, not
sufficient: two commits can define the same custom profile differently.

The command, mode, dataset, metadata, env, and invocation filters above apply
to both commit result sets before pairing.

The caller supplies `arm_key` with the typed `RunInvocation`, while parsed CLI
arguments are available. It excludes commit provenance, verbosity, and sample
count, while retaining measurement-affecting options. The comparison layer
treats it as an opaque identity and does not implement a second CLI grammar.
Historical rows have no `arm_key`; those rows use their exact stored
invocation string and can only pair when that spelling matches.

Pairing is by parsed command identity: two different brokkr commands that
happen to run the same subprocess stay separate rows.

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

`meta.`, `env.`, `prev.` and `provenance.` pairs are excluded as provenance:
they differ between two runs without the work differing, and `prev.*`
describes the preceding run so it differs on nearly every pair.

### Build provenance

A row whose binary brokkr built carries `provenance.*` pairs: what the build
was made with beyond what its commit records. Host side, read for the
directory cargo ran in - the compiler's `-V` line (`rustc`, chosen by cargo's
own precedence, `RUSTC` > `CARGO_BUILD_RUSTC` > `build.rustc` > `PATH`), the
rustflags cargo applies (`rustflags`) and which source supplied them
(`rustflags_source`: `env:RUSTFLAGS`, `env:CARGO_ENCODED_RUSTFLAGS`,
`target`, `build`, `none`, or `undecided` when a flagged `cfg(...)` table or
an unreadable config means no confident answer, and then no `rustflags` pair),
the codegen keys of every config file in the chain (`config`: `build.rustflags`
and its compiler, wrapper, target, rustdocflags and incremental keys,
`target.*.rustflags` and `.linker`, every `[profile.*]` table) and the
environment variables cargo reads for them (`env`: `RUSTFLAGS`, the compiler
and wrapper selectors, `CARGO_INCREMENTAL`, `CARGO_BUILD_*` for those keys,
`CARGO_PROFILE_*`, `CARGO_TARGET_*_RUSTFLAGS` / `_LINKER`). The resolved
`rustflags` follow cargo's merge: an array value appends to lower-precedence
files' and a string replaces them, the target source takes its triple table
before its matching `cfg(...)` tables, and a target source that comes to no
flags falls through to `build.rustflags`. The environment sources do not: a
set `RUSTFLAGS` or `CARGO_ENCODED_RUSTFLAGS`, empty or not, wins outright. Unit side, from cargo's own record of
the measured executable: `opt_level`, `debuginfo`, `debug_assertions`,
`overflow_checks`. A suite building several binaries attributes each row to
the build it measured - pbfhogg's osmpbf baseline is its own crate and records
its own - and a row measuring an installed tool (osmium, planetiler,
tilemaker) or a binary brokkr did not build records none. The namespace is
brokkr's alone: a `provenance.*` pair the measured program prints is dropped
with a warning. `brokkr results <uuid>` lists the pairs as `build ...` fields.

Differing provenance never splits a pair: what it covers is exactly what a
legitimate A/B varies (a toolchain-bump commit, a commit adding
`-Ctarget-cpu=native` to the repository's config), and the comparison must
stay possible. It is named instead:

```
  build provenance differs: rustflags -Ctarget-cpu=native -> , rustflags_source target -> none
  older pair with equal recorded build provenance: 1a2b3c4d vs 5e6f7a8b (host: memory 31000 MB vs 28000 MB (-3000 MB, -9.7%))
```

The compared rows are always the latest of each side. The second line, when
it appears, names the most recent older pair under the same key whose rows
both recorded equal provenance - most recent by row id, the pair whose older
row is newest - by short ids and with that pair's own host
differences - a pointer to a like-for-like reading, never substituted for the
one shown. A row without provenance (everything recorded before brokkr kept
it) is unknown, never equal to anything: one side missing prints `build
provenance: not recorded on A` (or `B`), and two unrecorded rows are never
offered as the like-for-like pair.
