# brokkr results

Query the results database, `.brokkr/results.db` - the durable record every
measured run writes. The sibling command `brokkr sidecar` queries the profiler
store (`sidecar.db`), and a script-bench child's evidence in the bench store;
see `docs/commands/measure.md` for what lands in which.
The current project's script-bench runs, kept in the per-user bench store,
are listed after these rows (Script benches, below).

`results` and `sidecar` open existing stores read-only and do not create
databases. A store on an older schema is migrated first (the same transactional
migrations a measured write applies, then the query proceeds read-only), so an
upgrade never makes existing history unreadable. A store stamped with a newer
schema version than this brokkr knows is refused, by queries and writes alike;
a stamp is never lowered.

Uniform across all projects, piners included (for its hotpath/alloc runs). The
exception is litehtml, whose only use of `results.db` is the unrelated
`MechanicalDb` schema: `results` and `sidecar` there read its script-bench
runs alone and never open that file, and `invalidate`, which reads nothing
else, is hidden from its `--help`.

## The table

Bare `brokkr results` prints the last `-n` rows (default 20), newest first. No
database yet, and no script-bench run of the project, reads as `no results yet
(run a benchmark first)` rather than an error - not having measured anything
is a normal state.

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
- `--command <SUB>` - substring, so `check` matches `check-refs` and
  `check-ids`.
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

A library measurement (`brokkr man library`, Measuring a scenario) stores
one group of rows under one uuid, its scenario row and one per call; its
uuid shows every row of the group as such a block, the scenario's first.
A library against CLI measurement (Isolation scenarios) adds a `/cli` row,
and its uuid shows the comparison of the call with its CLI counterpart
after the rows. A profiled one (`--hotpath`, `--alloc`; Profiling a
scenario) is the scenario row alone, with its profile.

A prefix matching several runs falls back to the table plus per-row details.
UUID resolution considers both stores; results lists result rows and reports
any matching sidecar-only UUIDs separately, then any matching script-bench
runs (Script benches, below); `sidecar` resolves across all three the same
way, and `invalidate` does not read the bench store. `sidecar` requires a
prefix to name one run and lists candidates when it is ambiguous. An empty
prefix is refused rather than matching every row.

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
arguments are available. It excludes brokkr's own commit provenance
(`--commit`), verbosity (`-v`, `--verbose`), and sample count (the `N` of
`--bench`, `--hotpath`, `--alloc`), while retaining measurement-affecting
options. Tokens the parse assigned to a command's trailing positional, the
argv `brokkr mogwai TARGET ARGS...` forwards to its child, are kept verbatim
whatever they spell, with or without a `--` before them: a child's `-v` or
`--bench 3` is part of what was asked. The key joins its tokens with single
spaces, unless one is empty or holds whitespace (or the first starts with
`[`), when it is the JSON array `["argv-v1", <tokens>...]` instead, so `"a b"`
and `a b` stay apart. The comparison layer treats the key as an opaque
identity and does not implement a second CLI grammar.
Historical rows have no `arm_key`; those rows use their exact stored
invocation string and can only pair when that spelling matches.

Rows stored before the positional boundary and the JSON form keep the key
they were given, stripped across the whole argv and space-joined. Where an
old row's child argv held one of the stripped spellings or a token with
whitespace, two things follow: it no longer pairs with a new row of the same
invocation, and it can still pair with a new row of a different one (an old
`stage -- quick -v` keyed as `stage -- quick` pairs a new `stage -- quick`).
Reading a row's `arm_key` beside its `brokkr_args` shows which case it is;
the argv alone does not, since the positional boundary is not stored.

A measurement-defining choice that reaches the subprocess argv from config
rather than from the brokkr argv cannot show in the argv key, so the row
carries it as a qualifier. For pbfhogg that is a dataset's `getid_ids` or
`getparents_ids` when it differs from the built-in list (`ids=...`), and for
`extract`, `multi-extract` and the suite's extract bench the bbox when it came
from the dataset rather than `--bbox` (`bbox=...`, the parsed coordinates, so
`12` and `12.0` pair), every dataset polygon the row extracted, in argv order
(`polygon=<name>@<xxh128>,...`: the argv names a polygon, not its bytes, and
the name keeps two polygons with the same bytes apart in the suite, whose rows
share one argv), and the strategy when one argv ran every strategy
(`--strategy all`, which is `extract`'s default, or the suite;
`strategy=...`), joined by `;` in that order. An extract
row from before the bbox and strategy qualifiers existed carries neither, so
it does not pair with a newer row that does. A pbfhogg `digest` without
`--form` carries the form it measured (`form=full` or `form=content`;
`docs/projects/pbfhogg.md`, Digest). An elivagar `tilegen` row, the `suite
elivagar` self arm's included, carries every flag its tilegen block and input
assertions passed (`tilegen=` and a JSON array of them, ocean entries under
their brokkr.toml names rather than the host's resolved paths; an empty block
on an input with neither assertion is `tilegen=[]`), since `brokkr tilegen`'s
own argv names only the input. A defaulted digest row or a tilegen row from
before these carries none and pairs only with other such rows, which mix the
forms or blocks of their day; an explicit `--form` digest row was never
qualified and pairs as before. Every `brokkr suite pbfhogg` row shares the
suite's argv, so a command preset whose command id another preset shares
(the `inspect`, `cat`, `tags-filter`, `getid` and `diff` families, the
preset named after the command included) carries `preset=<name>`, ahead of
the command's own qualifier and joined to it by `;`; a preset with no
sibling carries only the command's, so giving one a sibling later re-keys
it. A suite blob-filter row carries its sub-command (`command=cat-way`,
`command=inspect-tags-way`, ...). Before these, every preset of one command,
and every blob-filter sub-command on one input, shared one key, and
`--compare` kept whichever was stored last; those rows are not migrated, so
an older unqualified row of a shared command pairs with nothing newer, and
older rows pair among themselves as before. A pbfhogg `read`, `write` or `merge` row, the
suite's included, carries the cell it measured, since one invocation runs
several: `mode=...` for a read mode, `writer=...;compression=...` for a write
cell, `io=...;compression=...` for a merge cell. Before that every cell of an
invocation shared one key, so `--compare` paired whichever it met first. The
v27 results.db migration qualifies the older rows whose recorded `cli_args`
proves the cell, folding the qualifier onto the key they already compared
under (their `arm_key`, or `brokkr_args` where that is NULL), so those cells
stop colliding; an old row pairs with a new one only where their unqualified
keys already matched, which a row whose key still holds `--commit X` never
does. A row whose `cli_args` is missing, quoted or of an unrecognised shape
stays unqualified. A read, write or merge row stored under `--hotpath` or
`--alloc` before those modes profiled these cells carries no profile, and
qualifying it does not give it one. A `brokkr library` row's
qualifier is its library identity (the plan and context digests, the
dataset and generated identities, the harness features, and any seeds or
preparation; `brokkr man library`, Measuring a scenario), also stored as
`meta.library.identity`; a `/cli` row's is its CLI workload instead
(Isolation scenarios), so it pairs only with `/cli` rows.
When such a row is left without a pair, a line beneath it names the other
side's latest row of the same command, mode, input, captured env and build
and the first place their identities differ, down to the plan or context
field (the canonical documents are stored beside the rows), or says the
identities are equal and the invocations differ; it explains, and never
claims that difference is the only one. A qualified row
stores `["qualified-v1", <argv key>, <qualifier>]` (JSON) as its `arm_key`
and pairs only with rows carrying the same qualifier; an unqualified row's
key is the argv key alone, exactly as before qualifiers existed.

The rule behind the key: rows pair when brokkr asked for the same thing,
config-derived arguments included. What the measured program does with a
setting nobody passed is that revision's default, and a default that changes
between two commits is part of the code a comparison compares, even where it
changes output - except where brokkr holds a choice constant as a workload
axis, as it does pbfhogg's digest form, which is resolved before the run and
named in the row. A bare pbfhogg `inspect --tags`, which moved from counting
pairs to counting keys, is such a default; its pair workload is named only by
an explicit expression (`docs/projects/pbfhogg.md`, Inspect tags). Not in the key: the content behind an input's name (a
dataset PBF or an ocean file replaced under the same name), and a resource
the program sizes at run time (the worker count an omitted tilegen `threads`
resolves to), which the host-condition annotations below do not capture
either.

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

One `meta.` pair is the exception. A command whose exit status is its answer
(a project-defined set; for pbfhogg `diff`, `diff-snapshots`, `check-refs`,
`check-ids`, `check-index`, `check-locations`) stores a run that exits with an accepted non-zero code as a
completed run, and every such row records `meta.exit_code`, 0 included. When
both sides of a pair recorded one and they differ, the pair gets an
`answer: exit A -> B` line: one side found violations or differences and the
other did not, so the walls time different work. A row from before exit codes
were recorded has no key and is never flagged. `--meta exit_code=1` selects the
negative answers. An input that declares its answer in brokkr.toml stores
only that answer; the other fails the run (`docs/projects/pbfhogg.md`,
Declared answers).

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

## Script benches

Script-bench runs (`brokkr man bench-config`) are kept in one per-user store,
`$XDG_DATA_HOME/brokkr/bench.db`, shared by every project and every clone of
it; `results` shows the runs whose project is the one detected here. Every
view prints its typed output first, exactly as above, then the script-bench
section when it has runs to show; with none, the typed output is unchanged.
When the bench section has runs, the typed view's own empty answer (`no
matching results`, `(no results)`) is left out, and its other notices
(sidecar-only runs, the `--limit` cap) still print.

The store is opened read-only and never created. A store on an older schema
is migrated first, as results.db is; one stamped newer than this brokkr, or a
file at that path that is not a bench store, fails the command naming the
file, in every view: a history that cannot be read is not an empty one. With
no data home to name the store in (neither `XDG_DATA_HOME` nor `HOME`) there
is no store.

With `results_server` set in the user-wide config (`brokkr man config`), the
script-bench runs come from that server instead and the local file is not
opened. A server that cannot be reached, or answers what it was not asked,
fails the command naming the server: an unreachable history is not an empty
one either. A request that may succeed on a repeat is retried, a retry
starting only within twenty seconds of the first attempt and six attempts
at most, with a short growing pause; each attempt may itself wait as long as
the server's own deadline (five minutes and a proxy's allowance), so a
server that accepts connections and then stalls holds the command that
long. Each run listed is one request to the server.

**Filters.** A filter a run cannot answer excludes it, as `--meta` excludes a
row missing its key:

- `--commit` is a prefix of the run's full commit sha, ASCII case ignored.
- `--command` is a literal substring of the bench name, ASCII case ignored.
- `--env NAME=VALUE` matches a captured variable exactly.
- `--grep` / `--grep-v` are literal substrings of the run's invocation:
  each element of brokkr's argv, each element of every child's argv, each
  child's environment addition and each captured variable as `NAME=VALUE`.
- `--param NAME=VALUE` matches a bound param whose value, as the table lists
  it, is exactly VALUE: a scalar's key, a collection's keys joined by `,` in
  bound order (`--param osc=12,10` does not match `10,12`), an empty
  collection the empty string. It filters what is displayed, not a pairing
  identity: a scalar `""` and an empty collection both match `NAME=`, a
  scalar `a` and a one-key collection both match `NAME=a`, and a key
  holding a comma (bound only through `all`) matches like the keys its
  pieces would name. Each arm of a fan-out is a run of its own, so
  `--param` picks arms apart. It is case-sensitive, since keys are data, and
  excludes every typed row.
- `--mode`, `--dataset` and `--meta` have no meaning for a script bench and
  exclude every bench run.

**The table** lists runs newest first by start time (then uuid, so the order
is total whatever order runs were stored in), at most `-n`: uuid, start
(UTC), the commit's first eight digits (`-dirty` for a run of a dirty tree),
the bench, the result (the fastest iteration's wall at microsecond
precision, or `failed` / `interrupted` for a run without one), its params,
then a verdict column when some run has a verdict other than nil and an env
column when some run captured variables. Every attempted run is listed,
failed and interrupted ones included.

**A uuid prefix** matching exactly one run in any store prints that run as a
labelled block: identity and params (with each value's `from` path and file
digest), the outcome, the result's wall with every iteration's in execution
order and its verdict (nil, or the string quoted, so `""` stays visible),
host, conditions, captured env, the build asked for and as resolved, the
config digest, brokkr's argv, a `sidecar` field naming the `brokkr sidecar`
invocation that reads a child's samples, markers and counters (`brokkr man
measure`, Script-bench children) when some iteration launched one, then
each iteration with its outcome and each child with its start offset, wall,
ending, argv and what its capture kept and lost. Then the result's `record`
(for a run without a result, each iteration's that was extracted), one
entry per line as compact JSON. A prefix matching several runs prints the
table. A prefix that names no run of this project but names
another project's says which project it belongs to.

**`record.hotpath`.** A script that profiles puts the hotpath JSON report its
program wrote into `record.hotpath`, or calls it with `binary_with({hotpath
= true}, ...)`, which captures the report and fills `record.hotpath` when it
is the iteration's only one (`brokkr man bench`, `binary_with()`). It is rendered by the printer typed rows
use, capped by `--top`, and a pair holding one on both sides gets the
function diff after the comparison. A value with nothing recognizable as a
report is shown as JSON with a note; one only partly recognized renders what
was recognized, as for a typed row.

**`--compare A B`.** Each side is the completed runs of a clean tree whose
commit starts with that prefix and that pass the filters; a dirty run never
stands for a commit, and a run without a result never pairs or compares. Two
runs pair when their stored pairing keys are equal (`reference/bench-store.md`:
project, bench and every bound param with its keys and file digests in bound
order), and the latest run per key on each side is compared. Pairs are
ordered by bench, formed before `--limit` applies, and the cap is announced
as for typed pairs. What the key leaves out is named under the pair rather
than splitting it:

- `config differs:` the dotted paths of the stored config added (`+`),
  removed (`-`) or changed, at most six, then counted; a config that reads
  as neither TOML nor JSON is named by digest. The stored config is what the
  run was made from (`brokkr man bench`, What a run stores), so a moved
  setting shows as `config.<path>`, an edited script as `script.source` and
  a moved project root as `run.project_root`.
- `build differs:` the requested target, profile, features and default
  features, and the resolved bin and features; never the artifact digest,
  which differs across commits by construction.
- `host:` hostname, memory, governor and kernel, and `env:` the captured
  variables, as for typed pairs.
- `verdict:` when the results' verdicts differ, nil against `""` included:
  the sides measured different answers.
- `record:` the result records' entries that moved, `hotpath` aside.
- `other runs under this key with a different env or build:` how many runs
  each side holds under the key whose captured env or requested build differ
  from the compared one, naming the newest: an arm the pair collapsed.
- `older pair with equal captured env and requested build:` when the
  compared pair differs in either, the pair under the key whose older run is
  latest among pairs equal in both, with its walls and its own build and
  host differences. A pointer within the query, never substituted for the
  pair shown.

A run on one side only gets a line naming the other side's latest run of the
same bench and every identity difference from it (a param bound on one side
only, a collection against a scalar, values differing in key, order or
digest, or keys of different pairing algorithms): a candidate for why it has
no partner, not proof.
