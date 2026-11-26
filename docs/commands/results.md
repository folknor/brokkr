# brokkr results

`brokkr results` queries the results database, `.brokkr/results.db`, the
durable record every measured run writes. It then lists the current
project's script-bench runs, kept in the per-user bench store, after those
rows (see Script benches). This page covers the table, precision, filters,
`--grep`, a single run, `--compare` and script benches.

The sibling command `brokkr sidecar` queries the profiler store
(`sidecar.db`) and a script-bench child's evidence in the bench store;
`brokkr man measure` says what is written to which.

## Stores and schemas

`results` and `sidecar` open existing stores read-only and don't create
databases. The mark flags of `results` (see Marks) are its one write, to an
existing bench store.

brokkr migrates a store on an older schema first, with the same
transactional migrations a measured write applies, then runs the query
read-only. An upgrade never makes existing history unreadable. A store
stamped with a newer schema version than this brokkr knows is refused, by
queries and writes alike; brokkr never lowers a stamp.

The command behaves the same in every project except litehtml. There,
`results.db` holds only litehtml's own, unrelated schema, so `results`
and `sidecar` read litehtml's script-bench runs alone and never open that
file. `invalidate`, which reads nothing else, is hidden from litehtml's
`--help`.

## Table

Bare `brokkr results` prints the last `-n` rows (default 20), newest first.
With no database yet and no script-bench run of the project, it prints `no
results yet (run a benchmark first)` rather than an error: not having
measured anything is a normal state.

Two flags cap the output:

- **`--limit N`**: caps rows in the table and UUID-prefix views, and
  comparison pairs in `--compare` (default 20). brokkr forms compare pairs
  before applying the cap. A capped UUID-prefix view prints how many
  matching result rows it omitted, so the cap is visible.
- **`--top N`**: caps the functions shown in hotpath reports (default 10,
  `0` for all).

## Precision

Every row has an integer `elapsed_ms`. Most rows also have `elapsed_us`, the
exact wall in microseconds, from one of these sources:

- the external and hotpath paths, which time it themselves
- on older rows, the retired self-reported path, from a fractional
  `elapsed_ms=` line
- the stored distribution rows of the retired nidhogg `api` bench, which
  hold their fastest sample

Where `elapsed_us` exists, `elapsed_ms` is that value rounded to nearest.

Every view (the table, a single run, `--compare`) prints the microsecond
reading when a row has one, as milliseconds with three decimals (`0.312 ms`,
`6.847 ms`), and the integer otherwise (`1234 ms`). A sub-millisecond run
doesn't print as `0 ms`, and no two views disagree about the same row.

Rows recorded before a path stored microseconds have no `elapsed_us` and
print as integers. That detail was never measured, so brokkr never invents
it from `elapsed_ms * 1000`.

The same rule applies to each statistic of a stored distribution row.
Per-iteration walls keep their precision individually: exact samples print
with three decimals and older samples as integers. The list stays in
execution order.

## Filters

Every filter narrows the table, and they all AND together:

- **`--commit HASH`**: prefix match on the recorded commit, in both
  directions (4 or more digits). The recorded hash may start with what you
  typed, or what you typed may start with the recorded hash.
- **`--command SUB`**: substring, so `check` matches `check-refs` and
  `check-ids`.
- **`--mode SUB`**: substring; the exact values are `bench`, `hotpath` and
  `alloc`. `--variant` is accepted as an alias.
- **`--dataset SUB`**: substring against the input filename, so `eu`
  matches `europe-20260301-seq4714-with-indexdata.osm`.
- **`--meta KEY=VALUE`**: exact match on recorded metadata, the key written
  without its `meta.` prefix. Repeatable.
- **`--env NAME=VALUE`**: exact match on a captured env var, the name
  written without its `env.` prefix. Repeatable.

`--commit` matches both ways because each row has the width brokkr
abbreviated to when it was recorded, and that width varies. A full hash from
`git log` still finds a row stored at 7 digits. The same match selects each
side of `--compare`. A stored commit that isn't a hash at all, such as the
text `unknown`, never makes a query fail: brokkr shows the row with that
text verbatim.

Prefixes and substrings are literal: `_` and `%` match only themselves, and
ASCII case is ignored. `--meta` and `--env` compare the stored value as text
whatever type it was recorded as, so `--meta threads=8` matches an integer
`8`. A real matches SQLite's text form of it (`2.0`, not `2`).

`--meta` and `--env` exclude rows missing the key entirely. For an A/B where
one arm is defined by an unset variable, record an explicit baseline value
(`=0`) on the off runs rather than relying on absence. Or use `--grep-v`
(see grep), which can express absence.

These row filters also narrow UUID lookups and apply to both sides of
`--compare`, where the commit prefixes select the comparison axis.

## Redaction

Before persisting a row, brokkr redacts recognized credential-bearing argv
values and captured environment values, including URL credentials and
sensitive URL parameters. Only a credential name is redacted: `--token`,
`API_KEY=`, `--env PASSWORD=...`, an `Authorization` header. Ordinary
`--env` arms and paths are kept, so pairing and `--grep` still see them.

Upgrading the results store scrubs existing JSON argv and captured
environment entries. Older joined argv strings stay as recorded, because
their original argument boundaries can't be recovered reliably.

Global command-history argv uses the same credential rule and also hides the
value of its `--project-dir` option. Rows recorded under the broader
credential rule may already show `<redacted>` for ordinary `--env` values;
those values can't be recovered, so the stored redaction stays.

## grep

`--grep` and `--grep-v` match a substring against the run's invocation,
which spans three sources at once:

- the subprocess argv (`cli_args`)
- the brokkr argv (`brokkr_args`)
- each captured env var, rendered as `NAME=VALUE`

New `brokkr_args` values are JSON arrays, keeping spaces and punctuation
inside each argument; older rows keep their original joined string.

The third source is the point. An arm gated by an environment variable
appears in no argv at all, so `--grep LAYER_STATS` is the only way to select
it.

Both flags are repeatable, in the style of `git log --grep`: every `--grep`
must match, and any `--grep-v` hit excludes the row. They compose, which
makes the awkward A/B case expressible:

```sh
brokkr results --grep apply-changes --grep-v uring
```

That selects the arm distinguished only by an absent flag, which neither
`--grep` alone nor any `--env` filter can say. Both flags apply to
`--compare` as they do here, so you can narrow a comparison to one arm.

## Single run

When `brokkr results UUID_PREFIX` matches one result row, it prints a
labelled block instead of a table row. The block has the full multi-line
`cli_args`, the brokkr argv array, and the sidecar hint as a field. For a
`--bench N` row it adds the per-iteration walls and the `prev.*` provenance
of what ran immediately before: the neighbouring run is often the
explanation for an outlier. A stored distribution row (the retired nidhogg
`api` bench's) also shows its `samples`, `min`, `p50`, `p95` and `max`.

If `--stop` was configured, the block shows the requested marker separately
from the marker that stopped the child. A request that never fired doesn't
say the child was killed. Any row filters you give still apply to the UUID
matches.

Rows stored by the retired `brokkr library` still read:

- A measurement stored one group of rows under one uuid: its scenario row
  and one per call. Its uuid shows every row of the group as such a block,
  the scenario's first.
- A library-against-CLI group has a `/cli` row, and its uuid shows the
  comparison of the call with its CLI counterpart after the rows.
- A profiled measurement is the scenario row alone, with its profile.

A prefix matching several runs falls back to the table plus per-row
details. UUID resolution works across the stores this way:

- `results` lists result rows, then reports any matching sidecar-only UUIDs
  separately, then any matching script-bench runs (see Script benches).
- `sidecar` resolves across all three stores the same way. It needs a prefix
  that names one run, and lists candidates when the prefix is ambiguous.
- `invalidate` doesn't read the bench store.

An empty prefix is refused rather than matching every row.

A UUID with sidecar data but no results row reports itself as a sidecar-only
run (a dirty tree, or a run that failed before it could file a result) and
points at `brokkr sidecar`. Sidecar latest-keys such as `dirty` resolve to
the real UUID first, so an alias works wherever a prefix does.

## compare

`--compare A B` prints two commits side by side, one delta row per matched
pair:

```sh
brokkr results --compare COMMIT_A COMMIT_B
```

Two rows pair when this tuple agrees:

```text
(command, mode, input_file, arm_key, env_fingerprint, cargo_profile)
```

The command, mode, dataset, metadata, env and invocation filters apply to
both commits' result sets before pairing. Pairing is by parsed command
identity: two different brokkr commands that happen to run the same
subprocess stay separate rows.

`cargo_profile` is in the tuple because a config-selected profile
(a `[[bin]]` `profile`, say) changes the binary without changing the
argv. A row built under anything but release gets a `build:` line naming its
profile, so rows a profile split apart say why. A row whose profile is
missing or unreadable pairs with nothing, not even a row with the same text:
nothing is known about how either binary was built. Equal profiles are
necessary, not sufficient, since two commits can define the same custom
profile differently.

### Arm key

`arm_key` represents the measurement-affecting options, so arm-defining
flags like `--direct-io` keep different measurements apart without the
comparison reparsing argv. brokkr computes it from the parsed CLI arguments
when it records the row. The comparison treats the key as an opaque
identity and implements no second CLI grammar.

The key excludes these:

- brokkr's own commit provenance (`--commit`)
- verbosity (`-v`, `--verbose`)
- the sample count, the `N` of `--bench`, `--hotpath` and `--alloc`

Tokens the parse assigned to a command's trailing positional are kept
verbatim whatever they spell, with or without a `--` before them. That is the
argv `brokkr mogwai TARGET ARGS...` forwards to its child: a child's `-v` or
`--bench 3` is part of what was asked.

The key joins its tokens with single spaces. When a token is empty or holds
whitespace, or the first starts with `[`, the key is the JSON array
`["argv-v1", TOKEN, ...]` instead, so `"a b"` and `a b` stay apart.

Rows with no `arm_key` use their exact stored invocation string and pair
only when that spelling matches.

Rows stored before the positional boundary and the JSON form keep the key
they were given: stripped across the whole argv and space-joined. Where such
a row's child argv held a stripped spelling or a token with whitespace, two
things follow. It no longer pairs with a new row of the same invocation, and
it can still pair with a new row of a different one: an old
`stage -- quick -v` keyed as `stage -- quick` pairs a new `stage -- quick`.
Reading a row's `arm_key` beside its `brokkr_args` shows which case it is;
the argv alone doesn't, since the positional boundary isn't stored.

### Qualifiers

A measurement-defining choice that reaches the subprocess argv from config,
not from the brokkr argv, can't show in the argv key, so the row has it as a
qualifier. A qualified row stores `["qualified-v1", ARGV_KEY, QUALIFIER]`
(JSON) as its `arm_key` and pairs only with rows that have the same
qualifier. An unqualified row's key is the argv key alone.

brokkr no longer produces typed pbfhogg or elivagar rows, but stored ones
keep their qualifiers and the migration behavior below. Historical pbfhogg
rows are qualified by these, joined by `;` in this order:

- **`ids=...`**: a dataset's `getid_ids` or `getparents_ids`, when it
  differs from the built-in list.
- **`bbox=...`**: for `extract`, `multi-extract` and the suite's extract
  bench, the bbox when it came from the dataset rather than `--bbox`, as the
  parsed coordinates, so `12` and `12.0` pair.
- **`polygon=NAME@XXH128,...`**: every dataset polygon the row extracted, in
  argv order. The argv names a polygon, not its bytes, and the name keeps two
  polygons with the same bytes apart in the suite, whose rows share one argv.
- **`strategy=...`**: the strategy, when one argv ran every strategy
  (`--strategy all`, which is `extract`'s default, or the suite).

An extract row from before the bbox and strategy qualifiers has neither, so
it doesn't pair with a newer row that does.

Other historical rows are qualified this way:

- **Digest**: a pbfhogg `digest` without `--form` has the form it measured
  (`form=full` or `form=content`). An explicit `--form` digest row was never
  qualified and pairs as before.
- **Tilegen**: an elivagar `tilegen` row, including the `suite elivagar`
  self arm, has every flag its tilegen block and input assertions passed:
  `tilegen=` and a JSON array of them. Ocean entries appear under their
  brokkr.toml names, not the host's resolved paths, and an empty block on an
  input with neither assertion is `tilegen=[]`. `brokkr tilegen`'s own argv
  names only the input, hence the qualifier.
- **Suite presets**: every `brokkr suite pbfhogg` row shares the suite's
  argv. A command preset whose command id another preset shares has
  `preset=NAME`, ahead of the command's own qualifier and joined to it by
  `;`. That covers the `inspect`, `cat`, `tags-filter`, `getid` and `diff`
  families, the preset named after the command included. A preset with no
  sibling has only the command's qualifier, so giving one a sibling re-keys
  it.
- **Blob filters**: a suite blob-filter row has its sub-command
  (`command=cat-way`, `command=inspect-tags-way` and so on).
- **Cells**: a pbfhogg `read`, `write` or `merge` row, the suite's included,
  has the cell it measured, since one invocation runs several: `mode=...`
  for a read mode, `writer=...;compression=...` for a write cell, and
  `io=...;compression=...` for a merge cell.
- **Library**: a stored row of the retired `brokkr library` has its library
  identity as its qualifier, also stored as `meta.library.identity`: the
  plan and context digests, the dataset and generated identities, the
  harness features, and any seeds or preparation. A `/cli` row's is its CLI
  workload instead, so it pairs only with `/cli` rows.

A defaulted digest row or a tilegen row from before these qualifiers has
none. It pairs only with other such rows, which mix the forms or blocks of
their day.

Preset and blob-filter rows from before their qualifiers shared one key per
command or input, and `--compare` kept whichever was stored last. Those rows
aren't migrated: an older unqualified row of a shared command pairs with
nothing newer, and older rows pair among themselves as before.

Cell rows from before the cell qualifier shared one key per invocation. The
v27 results.db migration qualifies the older rows whose recorded `cli_args`
proves the cell. It folds the qualifier onto the key they already compared
under (their `arm_key`, or `brokkr_args` where that is NULL), so those cells
stop colliding.

An old cell row pairs with a new one only where their unqualified keys
already matched, which a row whose key still holds `--commit X` never does.
A row whose `cli_args` is missing, quoted or of an unrecognised shape stays
unqualified. A read, write or merge row stored under `--hotpath` or
`--alloc` before those modes profiled these cells has no profile, and
qualifying it doesn't give it one.

When a library row is left without a pair, a line beneath it names the other
side's latest row of the same command, mode, input, captured env and build.
The line names the first place their identities differ, down to the plan or
context field (the canonical documents are stored beside the rows), or says
the identities are equal and the invocations differ. It explains, and never
claims that difference is the only one.

### What the key covers

Rows pair when brokkr asked for the same thing, config-derived arguments
included. What the measured program does with a setting nobody passed is
that revision's default. A default that changes between two commits is part
of the code a comparison compares, even where it changes output.

The exception is a choice brokkr holds constant as a workload axis, as it
does pbfhogg's digest form, which it resolves before the run and names in the
row. A bare pbfhogg `inspect --tags`, which moved from counting pairs to
counting keys, is such a default; only an explicit expression names its pair
workload.

The key doesn't cover these, and neither do the host-condition annotations:

- the content behind an input's name, such as a dataset PBF or an ocean file
  replaced under the same name
- a resource the program sizes at run time, such as the worker count an
  omitted tilegen `threads` resolves to

### Annotations

brokkr annotates pairs whose host conditions differed (memory, governor,
kernel) rather than rendering a clean delta. The same numbers mean something
different across a governor change, and a comparison that hides that is
worse than none. brokkr annotates differing captured env the same way.

### Counters

Pairs whose stderr counters moved get one further line:

```text
    counters: cells_evaluated 5000 -> 4000, prints 1240 -> 1180
```

A wall on its own can't tell "the code got faster" from "the code did less".
This line turns "12% faster" into "12% faster on 8% fewer cells". A counter
present on one side only is reported the same way: instrumentation appeared
or vanished mid-series.

A moved counter is reported, never fatal. Doing less work is what most
optimization is, so a gate that fired on the first legitimate win would earn
a bypass flag and then be passed out of habit. Where a moved count does
invalidate a series, that is the project's rule to enforce unconditionally,
not a per-comparison verdict.

brokkr excludes `meta.`, `env.`, `prev.` and `provenance.` pairs as
provenance. They differ between two runs without the work differing, and
`prev.*` describes the preceding run, so it differs on nearly every pair.

One `meta.` pair is the exception. Historical typed answer commands (pbfhogg
`diff`, `diff-snapshots`, `check-refs`, `check-ids`, `check-index`,
`check-locations`) stored accepted non-zero exits as completed runs, and
every such row records `meta.exit_code`, 0 included. When both sides of a
pair recorded one and they differ, the pair gets an `answer: exit A -> B`
line. One side found violations or differences and the other didn't, so the
walls time different work.

A row from before exit codes were recorded has no key and is never flagged.
`--meta exit_code=1` selects the negative answers. These answer checks no
longer run; brokkr accepts their config expectations and ignores them.

### Build provenance

A row whose binary brokkr built has `provenance.*` pairs: what the build was
made with beyond what its commit records. The host side is read for the
directory cargo ran in:

- **`rustc`**: the compiler's `-V` line, the compiler chosen by cargo's own
  precedence: `RUSTC`, then `CARGO_BUILD_RUSTC`, then `build.rustc`, then
  `PATH`.
- **`rustflags`**: the rustflags cargo applies.
- **`rustflags_source`**: which source supplied them: `env:RUSTFLAGS`,
  `env:CARGO_ENCODED_RUSTFLAGS`, `target`, `build`, `none`, or `undecided`
  when a flagged `cfg(...)` table or an unreadable config means no confident
  answer, and then there is no `rustflags` pair.
- **`config`**: the codegen keys of every config file in the chain:
  `build.rustflags` and its compiler, wrapper, target, rustdocflags and
  incremental keys, `target.*.rustflags` and `.linker`, and every
  `[profile.*]` table.
- **`env`**: the environment variables cargo reads for them: `RUSTFLAGS`,
  the compiler and wrapper selectors, `CARGO_INCREMENTAL`, `CARGO_BUILD_*`
  for those keys, `CARGO_PROFILE_*`, and `CARGO_TARGET_*_RUSTFLAGS` and
  `_LINKER`.

The resolved `rustflags` follow cargo's merge. An array value appends to
lower-precedence files' values and a string replaces them. The target source
takes its triple table before its matching `cfg(...)` tables, and a target
source that comes to no flags falls through to `build.rustflags`. The
environment sources don't merge: a set `RUSTFLAGS` or
`CARGO_ENCODED_RUSTFLAGS`, empty or not, wins outright.

The unit side comes from cargo's own record of the measured executable:
`opt_level`, `debuginfo`, `debug_assertions` and `overflow_checks`.

A row measuring a binary brokkr didn't build records none. The retired
suites attributed each row to the build it measured, and their rows of
installed tools (osmium, planetiler, tilemaker) have none. The namespace is
brokkr's alone: brokkr drops a `provenance.*` pair the measured program
prints, with a warning, as it does one in any other brokkr namespace
(`brokkr man output-channels`). `brokkr results UUID` lists the pairs as
`build ...` fields.

Differing provenance never splits a pair. What it covers is what a
legitimate A/B varies, such as a toolchain-bump commit or a commit adding
`-Ctarget-cpu=native` to the repository's config, and the comparison must
stay possible. brokkr names the difference instead:

```text
  build provenance differs: rustflags -Ctarget-cpu=native -> ,
    rustflags_source target -> none
  older pair with equal recorded build provenance: 1a2b3c4d vs 5e6f7a8b
    (host: memory 31000 MB vs 28000 MB (-3000 MB, -9.7%))
```

The example wraps each line for width; brokkr prints each on one line.

The compared rows are always the latest of each side. The second line, when
it appears, names the most recent older pair under the same key whose rows
both recorded equal provenance, most recent by row id: the pair whose older
row is newest. It gives their short ids and that pair's own host
differences. It is a pointer to a like-for-like reading, never a substitute
for the one shown.

A row without provenance, as every row recorded before brokkr kept it is, is
unknown and never equal to anything. One side missing prints `build
provenance: not recorded on A` (or `B`), and two unrecorded rows are never
offered as the like-for-like pair.

## Script benches

Script-bench runs (`brokkr man bench-config`) are kept in one per-user
store, `$XDG_DATA_HOME/brokkr/bench.db`, shared by every project and every
clone of it. `results` shows the runs whose project is the one detected
here. Every view prints its typed output first, as above, then the
script-bench section when it has runs to show; with none, the typed output
is unchanged.

When the bench section has runs, brokkr leaves out the typed view's own
empty answer (`no matching results`, `(no results)`). Its other notices,
sidecar-only runs and the `--limit` cap, still print.

### Store

brokkr opens the bench store read-only and never creates it. A store on an
older schema is migrated first, as results.db is. A store stamped newer than
this brokkr, or a file at that path that isn't a bench store, fails the
command naming the file, in every view: a history that can't be read is not
an empty one. With no data home to name the store in (neither
`XDG_DATA_HOME` nor `HOME`) there is no store.

With `results_server` set in the user-wide config (`brokkr man config`),
the script-bench runs come from that server instead, and brokkr doesn't open
the local file. A server that can't be reached, or answers what it wasn't
asked, fails the command naming the server: an unreachable history is not an
empty one either.

brokkr retries a request that may succeed on a repeat, with a short growing
pause. A retry starts only within 20s of the first attempt, and there are
six attempts at most. Each attempt may wait as long as the server's own
deadline (five minutes plus a proxy's allowance), so a server that accepts
connections and then stalls holds the command that long. Each run listed is
one request to the server.

### Filters

A filter a run can't answer excludes it, as `--meta` excludes a row missing
its key:

- `--commit` is a prefix of the run's full commit sha, ASCII case ignored.
- `--command` is a literal substring of the bench name, ASCII case ignored.
  It matches the name a rename resolves the run to (see Marks), not the one
  it was stored under.
- `--env NAME=VALUE` matches a captured variable exactly.
- `--grep` and `--grep-v` are literal substrings of the run's invocation:
  each element of brokkr's argv, each element of every child's argv, each
  child's environment addition and each captured variable as `NAME=VALUE`.
- `--param NAME=VALUE` matches a bound param whose value, as the table lists
  it, is exactly VALUE (see the next paragraph).
- `--mode`, `--dataset` and `--meta` have no meaning for a script bench and
  exclude every bench run.

A param's listed value is a scalar's key, a collection's keys joined by `,`
in bound order (`--param osc=12,10` doesn't match `10,12`), or the empty
string for an empty collection. `--param` filters what is displayed; it is
not a pairing identity. A scalar `""` and an empty collection both match
`NAME=`, and a scalar `a` and a one-key collection both match `NAME=a`. A
key holding a comma (bound only through `all`) matches like the keys its
pieces would name.

Each arm of a fan-out is a run of its own, so `--param` picks arms apart.
`--param` is case-sensitive, since keys are data, and excludes every typed
row.

### Table

The table lists runs newest first by start time, then by uuid, so the order
is total whatever order runs were stored in, and shows at most `-n` of them.
Each row has these columns:

- uuid
- start (UTC)
- the commit's first eight digits (`-dirty` for a run of a dirty tree)
- the bench, as renamed (`concat (as cat)` for a run stored as `cat`)
- the result: the fastest iteration's wall at microsecond precision, a
  marked `measured` call's wall for a run that marked one, or `failed` or
  `interrupted` for a run without one
- its params
- a verdict column, when some run has a verdict other than nil
- an env column, when some run captured variables
- a `hidden` column, when some listed run is hidden (`retired`,
  `invalidated`, or both; see Marks)

Every attempted run is listed, failed and interrupted ones included, unless
a mark hides it.

### Lookup

A uuid prefix matching exactly one run in any store prints that run as a
labelled block. A uuid finds a hidden run without `--hidden`. A prefix
matching several runs prints the table. A prefix that names no run of this
project but names another project's says which project it belongs to.

The block's run-level fields are these, in order:

- **Identity**: the bench as renamed, with the name it was stored as when
  that differs.
- **`hidden`**: one field per mark hiding the run: what, when, on which
  host, why, and the mark's uuid.
- **Params**: each value with its `from` path and file digest.
- **Outcome and result**: the outcome, the result's wall with every
  iteration's in execution order, and its verdict (nil, or the string
  quoted, so `""` stays visible). When the run marked a measured call, each
  wall is that call's, said so, with the block's wall beside it in the
  iteration lines and the child flagged `(measured)`.
- **Host and conditions**.
- **`history`**: the last brokkr command in your history before the run and
  how long before the run it ended, and how long before the run
  `fstrim.timer` last fired.
- **Captured env**.
- **Build**: as asked for and as resolved, with the root `Cargo.lock`'s
  source and its digest after the build (`brokkr man run`, `--commit`).
- **`rustflags`**: for a bench with `rustflags` (`brokkr man bench`, The
  build): the resolved tokens and how they were injected (the env variable
  and the whole value cargo received, or the `--config` arguments), or that
  the build stopped before they were placed.
- **Config digest and brokkr's argv**.
- **`sidecar`**: when some iteration launched a child, the `brokkr sidecar`
  invocation that reads its samples, markers and counters (`brokkr man
  measure`, Script-bench children).

Then come the iterations, each with its outcome and its host lines:

- the sampled device
- the gap from the cell before it
- its starting `Dirty` and `Writeback`
- a `block` line of their peaks over the readings inside the block, with the
  device's writes, discards and mean block-layer write wait per completed
  request
- or, read without its periodic readings or with no block, an `around` line
  from the start and end readings, setup and extraction included

Each figure is `?` or left out when its readings can't give it. brokkr
places the block window through one clock anchor taken before `bench()`, so
a host suspend during the block misplaces the window by its length.

Each child of an iteration shows:

- its start offset, wall, ending and argv, and what its capture kept and
  lost
- for a `program` call, the executable it ran and that file's digest, or why
  it wasn't read
- for a call marked `hotpath`, its stage, occurrence and capture status
- for a child whose stderr was kept, its size and state and the last lines
  of its stored tail: at most 10 lines and 2000 characters, control
  characters escaped
- for a call that prepared inputs, its page-cache account: the procedure,
  the scan interval before launch, and each file's mode, paths and resident
  bytes of its size

A `--hotpath` run says `measured hotpath`, an `--alloc` run `measured
alloc`. The block ends with the result's `record`, one entry per line as
compact JSON; for a run without a result, each extracted iteration's record.

### JSON

`brokkr results UUID --json` prints the one script-bench run the prefix
names, hidden or not, as a JSON document on stdout, for a project tool to
read and gate on. A prefix naming no run, or several, fails the command.
Typed rows have no JSON form, and `--json` takes no filter.

The document's `schema` is `brokkr-results-run` and its `version` is an
integer. A change that would make a reader misread the document raises the
version, so a tool should refuse one it doesn't know. A new field beside the
others doesn't raise it.

Top-level fields:

- **Identity**: `uuid`, `project`, `bench` (as renamed), `stored_bench`,
  `commit`, `dirty`, `hostname`, `started_unix_ms`, `measurement`.
- **`build`**: `target`, `profile`, `features`, `default_features`,
  `resolved`.
- **`outcome`**, **`iterations_requested`**, **`pairing_key`**.
- **`hidden`**: one entry per mark hiding the run, with `by` (`retired` or
  `invalidated`), `mark`, `at_unix_ms`, `hostname` and `reason`.
- **`result`**: `null` for a run without a result; otherwise `iteration`
  (its index), `wall_us`, `wall_of` (`block`, or `measured_child` for a run
  that marked a measured call) and `verdict`. This is the iteration the
  table and `--compare` use, so a tool needn't pick a best of N itself.
- **`iterations`**: in execution order, each with `index`, `result`
  (whether it is the result iteration), `outcome`, `wall_us`,
  `result_wall_us`, `record` and `children`.
- **`document`**: the stored run document whole, every sample, record and
  host reading included. Its own `format` field versions it.

Each child has `index`, `program`, `argv`, `measured`, `wall_us`, `ending`,
`exit_code` (`null` unless it exited), `samples` (the sidecar sample count),
and `peak_rss_kb` and `peak_vm_hwm_kb`, the largest `rss_kb` and `vm_hwm_kb`
over its samples, `null` with none.

### Hotpath reports

brokkr renders each report a marked call captured (`brokkr man bench`,
Hotpath capture) with the printer typed rows use, capped by `--top`. Each is
shown under its stage and occurrence, as `hotpath decode #1 (iteration 2)`:
the result iteration's, or for a run without a result every iteration's. A
report with nothing recognizable is named instead.

A script may also put a report into `record.hotpath` itself, which brokkr
never does. brokkr renders it the same way, and shows a value with nothing
recognizable as a report as JSON with a note. A report only partly
recognized renders what was recognized, as for a typed row.

### Compare

Under `--compare A B`, each side is the completed runs of a clean tree whose
commit starts with that prefix and that pass the filters. A dirty run never
stands for a commit, and a run without a result never pairs or compares.

Two runs pair when their stored pairing keys are equal, and brokkr compares
the latest run per key on each side. The key
is the project, the bench, every bound param
with its keys and file digests in bound order, and what was measured:
timing, `--hotpath`, `--alloc`, or incidental, a timing run that captured a
report anyway.

Across a rename (see Marks), runs pair under the name the chain resolves
to: brokkr compares the stored key with its bench replaced by that name.
After `--rename cat concat`, runs stored as `cat` and as `concat` are one
history. A `stored as: A cat, B concat` line names the stored names whenever
either compared run's differs from the one shown.

Pairs are ordered by resolved bench and formed before `--limit` applies, and
brokkr announces the cap as for typed pairs. What the key leaves out is
named under the pair rather than splitting it:

- **`config differs:`**: the dotted paths of the stored config added (`+`),
  removed (`-`) or changed, at most six, then counted. A config that reads
  as neither TOML nor JSON is named by digest. The stored config is what the
  run was made from (`brokkr man bench`, What a run stores), so a moved
  setting shows as `config.PATH`, an edited script as `script.source` and a
  moved project root as `run.project_root`.
- **`annotations:`**: how the bench's `description`, `version`, `date` and
  `changelog` (`brokkr man bench-config`, Annotations) differ. See the
  annotations paragraph after this list.
- **`build differs:`**: the requested target, profile, features and default
  features, and the resolved bin and features; never the artifact digest,
  which differs across commits by construction.
- **`lfs pointers on A`** (or `B`): that side's `--commit` worktree held
  tracked files left as Git LFS pointers, the first by path and how many
  more, so its numbers may be of a build or script that read pointer text.
  Silent for a run with none and for one that recorded nothing.
- **`root Cargo.lock differs:`**: both sides recorded the digest of the
  build root's `Cargo.lock` after the build and the bytes differ, with each
  side's source (`live`, `tracked`, `seeded` from the live tree, `retained`,
  `fresh`). It says the file differs, not that a dependency version did: a
  workspace package's own entry changes it too. Silent when either side
  recorded no digest (an unmanaged layout, no file, an older run).
- **`rustflags differ:`**: the sides' `rustflags` recipes or their injection
  differ. Each side's tokens and injection are shown, a side without one as
  the plain build: a layout pin against no pin, or an inherited `RUSTFLAGS`
  that changed under the same tokens. Pairing ignores it, since the flags
  may be what is compared.
- **`executables differ or were not read:`**: see the executables paragraph
  after this list.
- **`page-cache preparations differ:`**: when either side prepared inputs
  `cold` or `warm` (`brokkr man bench`, Page cache) and the sides' sequences
  differ. A sequence is, per iteration, each child in launch order with its
  measured mark and each prepared file's mode and paths, or `none`; each
  side shows its distinct sequences. Resident counts and scan times are
  observations and never compared.
- **`host:`** and **`env:`**: hostname, memory, governor and kernel, and the
  captured variables, as for typed pairs. A script-bench `host:` also
  names, from each side's result iteration (`brokkr man bench`, What a run
  stores), where its starting `Dirty` and `Writeback` differ and where its
  gap from the cell before it differs: the predecessor, the elapsed time,
  and what the device wrote and discarded in the gap.
- **`verdict:`**: the results' verdicts differ, nil against `""` included:
  the sides measured different answers.
- **`record:`**: the result records' entries that moved, `hotpath` aside.
- **`hotpath captures unpaired:`**: a stage and occurrence one result
  iteration captured and the other didn't. Every capture both sides have
  gets its function diff after the comparison, as `record.hotpath` held on
  both sides does.
- **`other runs under this key with a different env or build:`**: how many
  runs each side holds under the key whose captured env or requested build
  differ from the compared one, naming the newest: an arm the pair
  collapsed.
- **`older pair with equal captured env and requested build:`**: when the
  compared pair differs in either, the pair under the key whose older run is
  latest among pairs equal in both, with its walls and its own build and
  host differences. A pointer within the query, never a substitute for the
  pair shown.

The `annotations:` line reads each side's stored config on its own, under
the name that side was stored as: `version 2 -> 3`, `date ... -> ...`, a
description added, removed or changed, and a changelog added, removed or
changed with its entry counts, any edit to an entry counting. A field
brokkr would refuse today is named `(invalid: why)` whenever either side
holds one, the same on both sides or not. A side whose stored config can't
be read is named unreadable, with what the other side holds. The line is
descriptive only: brokkr claims no cause for the numbers from it. The same
changes still count among the `config differs:` paths.

The `executables differ or were not read:` line names, by path, each
external program a `program` call ran (`brokkr man bench`, `binary_with()`)
whose observed digests differ between the sides, that one side never ran,
or that either side couldn't read. An unread file is no evidence it stayed
the same: a system `lua5.4` upgraded between the runs shows here. Every
iteration's observations count, so a program that changed mid-run lists
both digests. brokkr compares a path as resolved, its `.` components
dropped, so a program reached through a different directory on `PATH` is a
different path.

A run on one side only gets a line naming the other side's latest run of
the same resolved bench, with the name it was stored as when that differs.
The line lists every identity difference from that run: a param bound on one
side only, a collection against a scalar, values differing in key, order or
digest, a different measurement class, or keys of different pairing
algorithms. It is a candidate for why the run has no partner, not proof.

Under `--hidden`, a hidden run takes part as any other. brokkr compares the
newest run per key whether hidden or not, so a hidden run can stand in for
an older visible one, and a `hidden on A` or `hidden on B` line names the
compared run and the mark hiding it.

### Invocation

`--invocation ID` summarizes one `brokkr bench --rounds` invocation
(`brokkr man bench`, Rounds), named by a prefix of its id, on the plan its
runs have. brokkr reads every run of it, hidden or not, at one revision. No
other filter applies, and each conflicts with `--invocation`, since a cell
filtered away would read as missing. brokkr doesn't open results.db.

A prefix naming several invocations lists them, and one naming none says
so. brokkr refuses these as not one invocation's runs:

- runs that disagree on the plan
- two runs for one slot
- a cell of a build stored as failed
- cells of one build recording different builds
- a cell after the stop a run records
- two runs recording a stop

A plan stored before builds were per cell, with one build per competitor,
reads as that build in every workload.

The summary's header names these:

- the benches, start and host, the axis, rounds and workloads
- each competitor with its commit and, when every workload runs the same
  builds, its build
- each build's `rustflags` tokens, as its failed build run or first cell
  recorded them; a build no stored run records, none of its cells having
  run, is named as unknown
- builds of one tree (checkout and commit) whose root `Cargo.lock` digest
  differs from the first build of that tree to record one, and, with
  several builds, those that recorded none
- each build that failed or was interrupted
- how many runs a mark hides, and whether a cell ran a dirty tree

Then, per workload, headed with its builds when they differ by workload:

- each competitor's median result over its cells with a result (an even
  count's middle two averaged, rounded down), against the rounds planned
- each later competitor against the first, over the rounds where both hold
  a result: how often it was faster, slower and tied, of how many, and the
  median paired difference

When the results aren't one kind, such as a different pairing class or a
measured call's wall against a block's, nothing is compared and the summary
says why.

The summary ends with the params whose files or `from` path differ between
competitors under the same key, the competing param aside, and a param bound
on one side only named so. Then each planned cell without a result: failed
or interrupted (with its run), blocked by a failed build, not run because
the invocation stopped at a run it names, or absent, which says nothing of
why (a crash, a publication still in doubt). The summary is derived, never
stored.

### Marks

What is said about runs after the fact is stored in the bench store beside
them. It is never in `brokkr.toml` and
never an edit of a run, so it holds locally and through a results server
alike. The mark flags are these:

- **`--retire NAME --reason TEXT`**: retires a bench name. Its runs, and any
  published under it later, are left out of the table and `--compare`. You
  may then delete the bench from `brokkr.toml` and its runs stay hidden.
  Refused for a name the store doesn't know or one already retired.
- **`--unretire NAME`**: undoes it; refused for a name not retired.
- **`--invalidate UUID --reason TEXT`**: invalidates one run. It is left out
  of the table and `--compare`, and a uuid lookup still shows it, marked.
  Refused for a run already invalidated.
- **`--revalidate UUID`**: undoes it; refused for a run not invalidated. A
  run of a retired bench stays hidden.
- **`--rename OLD NEW`**: renames a bench; see the next paragraph. Takes no
  `--reason`.
- **`--hidden`**: lists and compares hidden runs too, each marked. It leaves
  typed rows as they are.

After `--rename OLD NEW`, runs stored under OLD, or under any name renamed
to it, list, filter, pair and compare as NEW. You can then rename the
bench's table in `brokkr.toml` without orphaning its history. OLD is the
name its runs resolve to now: after `a -> b`, rename `b`, not `a`. A name is
renamed at most once. A run published later under NEW joins the renamed
history.

`--rename` is refused for a name the store doesn't know, and onto a name it
knows (one with runs, or named by a mark), so histories never merge
silently. The exception is a rename onto a name renamed into OLD: that is
the undo, and it makes every name on the path its own bench again. Renaming
back undoes a rename, so there is no `--unrename`. A rename never changes
what is hidden: a retirement follows it, and an undo leaves each name of the
path retired as the end was.

A mark flag takes no other option. It prints the mark it recorded, not what
is visible now: an unretired bench's invalidated runs stay hidden. brokkr
retries a mark as it does a publication. One whose outcome stays unknown
says so, and running the command again is a new request rather than a retry
of it. A local mark needs a bench store to exist and never creates one.

The UUID of `--invalidate` and `--revalidate` is a prefix, as for a lookup.
It must name exactly one script-bench run of this project, hidden or not,
whichever flag it is given to. A prefix naming several is refused with every
full uuid it names, and one naming none says so; typed rows aren't marked.
brokkr fixes the run the prefix names before it mints the request, and the
stored mark has its full uuid, so a run published later never changes what
the request names.
