# Ratatoskr bench gate

The bench gate exists only in `project = "ratatoskr"`. It sits on top of
`sync --bench` (`brokkr man sync`) to catch performance and correctness
regressions against a per-hostname pinned baseline. The gate compares scalar
metrics from the current run to a baseline row recorded in
`.brokkr/ratatoskr/gate.db`, and exits non-zero on any threshold breach.
This page covers the store, the write policy, the config, baseline lookup,
rules and selectors, running one gate or all of them, and `--as-baseline`.

For `[ratatoskr.gate.*]` configuration, see `brokkr man ratatoskr
configuration`. For the underlying bench mechanics (the saehrimnir spawn,
FIFO markers, best-of-N selection, `summary.json` ingestion), see `brokkr
man sync bench`.

## Storage

`.brokkr/ratatoskr/gate.db` is a local SQLite database, following the same
convention as `.brokkr/results.db`. Its schema is one table:

```sql
CREATE TABLE gate_runs (
  uuid        TEXT PRIMARY KEY,
  created_at  INTEGER NOT NULL, -- unix seconds
  git_commit  TEXT NOT NULL,    -- the real short SHA; tree state in dirty
  dirty       INTEGER NOT NULL, -- 0/1
  hostname    TEXT NOT NULL,    -- libc gethostname
  gate_name   TEXT NOT NULL,    -- e.g. "jmap_small"
  script      TEXT NOT NULL,    -- project-relative; older rows absolute
  fixture     TEXT NOT NULL,
  profile     TEXT NOT NULL,    -- dev/release/cargo:NAME
  elapsed_ms  INTEGER NOT NULL,
  exit_code   INTEGER NOT NULL,
  success     INTEGER NOT NULL, -- 0/1
  sidecar     TEXT NOT NULL,    -- JSON blob
  meta        TEXT NOT NULL,    -- JSON blob from summary.json ingestion
  features    TEXT              -- harness cargo features; NULL = pre-v2 row
);
```

An index on `(gate_name, hostname, created_at)` serves the lookups below.

`features` is the `[ratatoskr.harness] features` list the harness was built
with, in canonical form. brokkr splits entries on `,` and whitespace, as
cargo accepts, then sorts, deduplicates and joins the names with `,`, so
list order and shape never read as a different build. The empty string is a
default-features build. The schema v1 to v2 migration leaves existing rows
NULL rather than backfilling: the features an older row was built with are
unknowable, and a guessed value would claim an identity it may not have.

Current rows store an in-project script path relative to the project root;
scripts outside the project stay absolute. Older rows hold canonical
absolute paths. When a checkout has moved, the gate accepts an older
absolute baseline path whose components end in the current project-relative
path.

`profile` holds Cargo's profile name (`dev` or `release`), or `cargo:NAME`
for a custom profile, so a custom profile named like another stored value
can't be read back as it. brokkr reads the older spelling `debug` as `dev`.
Any other value is an unknown profile, and a baseline that has one is
refused.

## Write policy

After all harness iterations succeed, the gate hook validates the run and
inserts its row before looking up a baseline or evaluating rules. A metric
breach or a missing baseline therefore leaves the inserted row for you to
inspect. A harness failure or an earlier hook error writes no row.

`--as-baseline` uses the same insert path and skips only evaluation.
Baselines are pointers in TOML and don't change the write path.

## Config

A gate is a `[ratatoskr.gate.NAME]` table with a script, a per-host baseline
map and metric tables. This example declares one:

```toml
[ratatoskr.gate.jmap_small]
script = "crates/app/tests/sync-harness/jmap-initial.lua"
baseline_label = "2026-05-08 jmap_small green"   # optional note

[ratatoskr.gate.jmap_small.baseline]
folk-desktop = "a344fcc2"
ci-linux-x64 = "81d03b7a"

[ratatoskr.gate.jmap_small.metrics.elapsed_ms]
max_relative = 1.10
max          = 5000

[ratatoskr.gate.jmap_small.metrics."sidecar.rss_peak_kb"]
max_relative = 1.15
```

Further metric tables follow the same shape:

```toml
[ratatoskr.gate.jmap_small.metrics."meta.provider_requests"]
max_delta = 0

[ratatoskr.gate.jmap_small.metrics."meta.message_count"]
equal_to_baseline = true

[ratatoskr.gate.jmap_small.metrics."meta.correct"]
equal = 1
```

`baseline.HOSTNAME` is the source of truth, with no implicit fallback.

## Baseline lookup

The gate finds its baseline in four steps:

1. Read the current hostname (libc `gethostname`).
2. Look up `[ratatoskr.gate.NAME.baseline].HOSTNAME`. If it is missing,
   fail with `no baseline pinned for host "<hostname>" in gate "<name>" -
   record one with --as-baseline and add it to brokkr.toml`.
3. Look up that UUID in `gate.db`, scoped to this hostname, as described
   after this list.
4. Check that the row's `gate_name`, `script`, `fixture`, `profile` and
   `features` match the current invocation, as described after this list.

The pin in step 3 is a non-empty lowercase hexadecimal UUID or a prefix of
one, up to 32 characters; `a344fcc2` is a short form. It must resolve to
exactly one row:

- an exact match wins
- a prefix matching a single row resolves to it
- a prefix matching several is a hard error naming them

The lookup is never newest-wins, which would let a later run sharing the
prefix silently become the baseline. An empty pin is refused, and matching
is byte-exact: `%` and `_` aren't wildcards. The lookup excludes the run
being evaluated. That run is recorded before the lookup, and a pin that
matched it would make it its own baseline, where `max_delta = 0` and
`equal_to_baseline` pass by construction.

If no row matches, the lookup fails. The failure distinguishes three
conditions, because the remedies differ and only the lookup's absence is a
fact:

- the UUID exists under another hostname: the pin is filed under the wrong
  host key
- the UUID is absent and the gate has no other rows on this host: the local
  `gate.db` looks reset, relocated or newly created
- the UUID is absent but the gate does have other rows here: that row is
  gone, or was written to a different `gate.db`

In every case the message warns that `--as-baseline` isn't a safe fix.
Someone pinned that UUID deliberately, and re-recording rebases every rule
onto the current tree's numbers. On a `max_delta = 0` rule that silently
blesses whatever regression is in the tree and leaves the gate permanently
blind to it. Re-record only from a tree independently confirmed good.

A mismatch in step 4 is a hard error. A `--debug` run against a release
baseline, the reverse, or any two different profiles would make every
timing rule meaningless, and a baseline built with a different feature set
measured a different binary.

A baseline whose `features` is NULL, recorded before the column existed, is
compared anyway, with a warning that the features identity went unchecked.
Refusing it would force a re-record of every older pin, which is the blind
rebase the `--as-baseline` warnings exist to prevent. Repin from a tree
confirmed good to make the check apply.

## Bisecting

`sync SCRIPT --bench N --gate NAME --commit REF` evaluates the gate against
a harness built from a worktree at `REF`, so you can walk a breach back to
the commit that introduced it. The script and fixture don't move with the
build (`brokkr man sync bench-commit`). So the baseline's pinned `script`
path keeps matching across refs; the identity check in step 4 of Baseline
lookup would otherwise refuse every `--commit` run.

A detached worktree is clean by construction, so `--commit` also gives
`--as-baseline` an unambiguous provenance. The row records the ref that was
built, and no `--force` is involved, so it never has the dirty tag.

## Store lifetime

`gate.db` lives at `PROJECT_ROOT/.brokkr/ratatoskr/gate.db`: per project,
never global. It holds the only copy of every baseline's measured numbers.
`brokkr.toml` stores nothing but the UUID pointer, so a lost row can't be
reconstructed from git.

So `gate.db` has no retention or pruning policy; the store is
insert-and-select only. `brokkr clean` doesn't touch it: a clean removes the
artefact and `mock/` directories under `.brokkr/ratatoskr` and spares every
file at that level. `brokkr kill`'s graceful path runs no clean, so a
SIGTERMed bench keeps its artefact directories as well as the baselines.

The sidecar backups under `$XDG_DATA_HOME/brokkr/sidecar-backups/` are
sidecar profile data only. They are unrelated to gate baselines and no
recovery path for them.

## Dirty trees

The clean-tree demand exists so the commit a baseline is pinned to describes
the code that produced the numbers. The check doesn't count these against
you:

- `.brokkr/`: every store in there, `gate.db` included, is an output of the
  run being measured, so it can't invalidate the pin
- `*.md`
- `brokkr.toml`, which matters here because pinning a baseline means editing
  it

Without those exclusions `--as-baseline` would block itself. Each gated run
can write a `gate.db` row, which dirties the tree wherever a project tracks
that file, so every recording after the first would refuse until you
committed. `brokkr approve` has the same exemption for `approved.png`.

A dirty-tree run under `--force` can still reach the gate hook. If the hook
inserts its row, that row has `dirty = 1` and the run prints a warning. The
harness's earlier `dirty tree - results will NOT be stored in database`
warning concerns `results.db`. Dirty runs get no results row, while a
successful gate-hook insert is stored independently, so a metric-rule
failure stays inspectable. Harness failures and other errors before
insertion produce no gate row.

`--force` keeps the real `git_commit` SHA in the row, so the SHA stays
useful for forensics. Sidecar data stays reachable with `brokkr sidecar
dirty`. You may pin a dirty row as a baseline; the gate report flags it,
and every later comparison against it warns again.

## Rules

Each metric sub-table accepts one or more of these rules:

- **`max = SCALAR`**: hard cap; the current value must be `<=` it
- **`min = SCALAR`**: hard floor; the current value must be `>=` it
- **`max_relative = FACTOR`**: current `<=` baseline `*` factor
- **`min_relative = FACTOR`**: current `>=` baseline `*` factor
- **`max_delta = SCALAR`**: current `-` baseline `<=` delta
- **`equal = SCALAR`**: literal equality with the given scalar
- **`equal_to_baseline = true`**: current must equal baseline exactly

Every rule on a metric applies (logical AND). All comparisons are scalar;
there is no list or object diffing.

Before building the harness, brokkr compiles the configured metric names
into typed selectors and the configured rules into predicates. It rejects
these before any build work:

- an unknown bare key
- an empty `sidecar.` or `meta.` key
- a non-scalar `equal` value
- a metric table with no effective predicate: none of these rules, or only
  `equal_to_baseline = false`, which would check nothing and report PASSED
- a gate with no metric tables at all, unless the run is `--as-baseline`

## Selectors

A metric name is a selector in one of three namespaces:

- **Bare keys**: the fixed set `elapsed_ms`, `exit_code` and `success`,
  which map to top-level `gate_runs` columns. Adding a bare key is a schema
  migration.
- **`sidecar.KEY`**: a flat lookup into the sidecar JSON blob.
- **`meta.KEY`**: a flat lookup into the `summary.json` ingestion blob.
  Numeric and boolean scalars work with numeric predicates, booleans as `1`
  or `0`; string scalars work with `equal = "..."`.

The gate hook runs only after successful iterations, so `exit_code` is
always `0` and `success` always `1` for rows it evaluates. Predicates on
them confirm the recorded successful-run shape but can't detect a benchmark
failure.

Quoted dotted keys in TOML (`"sidecar.rss_peak_kb"`) keep the namespace
prefix readable. brokkr checks selector syntax before the build. Missing
keys and null, array or object values are errors at gate time.

The sidecar projection emits zero for `num_threads_max`, `read_bytes`,
`write_bytes`, `vol_cs`, `nonvol_cs`, `minflt` and `majflt` when the sidecar
has no samples. Other missing selectors aren't filled with a default.

## Running a gate

One gate runs with this invocation:

```text
brokkr sync SCRIPT --bench [N] --gate NAME [--force] [--keep-artefacts]
    [--debug | --release | --profile NAME]
```

`--gate` requires `--bench`: every rule compares measured numbers, so there
is nothing to evaluate on an unmeasured run.

The bench runs as `brokkr man sync bench` describes. After a completed
successful bench, brokkr writes a row to `gate.db` and evaluates every rule
under `[ratatoskr.gate.NAME.metrics.*]`. It reports each rule as `OK` or
`FAIL` with a numeric line, and exits non-zero if any rule fails.

## gate all

`--gate NAME` runs exactly one gate. The reserved value `all` runs every
configured gate in name order, each supplying its own script from config.
The sweep takes no SCRIPT argument, and passing one is an error rather than
a silently ignored contradiction.

brokkr validates the shared mock and harness config, every gate script with
its frontmatter and fixture, and each rule set before anything is built. A
typo in a later gate shows at once rather than after earlier benches have
run. Each rule set must be non-empty, and every rule must have a predicate.

A breach doesn't stop the sweep. After a refactor the useful answer is the
whole blast radius, not the first provider that tripped. Every gate runs,
brokkr collects each failure, and the command exits non-zero listing all of
them.

An interrupt isn't a breach. `brokkr kill` or Ctrl-C stops the sweep at the
current gate, prints how far it got, and exits as interrupted, rather than
recording a FAIL and carrying on under the lock.

The sweep holds the global lock for its whole duration, not per gate. The
per-gate bench still acquires internally, but the lockfile is re-entrant
within the process, so those acquires join the sweep's hold rather than
releasing between gates. This matters for comparability against the pinned
baselines. With per-gate locking, another brokkr invocation could take the
lock between two gates and run a build or bench, contaminating the next
gate's timing. A contaminated number still goes into `gate.db` and is
evaluated against the baseline, so its breach would be indistinguishable
from a real regression. Holding across the sweep also keeps the cohort
contiguous in time.

Two gates may reference the same script. The hazard there is implicit
selection, a script silently dragging in several gates, and an explicit
sweep names what it runs and reports each by gate name.

## as-baseline

`--as-baseline` records the row as usual but suppresses gate evaluation. It
prints the new UUID and the exact TOML line to paste:

```toml
[ratatoskr.gate.jmap_small.baseline]
folk-desktop = "a344fcc2"
```

brokkr never edits `brokkr.toml` itself. Promotion is always a manual paste,
so the diff goes into a normal commit.

When the gate already pins a baseline for this host, `--as-baseline` warns
first. Repinning re-anchors every baseline-relative rule onto the run in
front of it, so a regression present at that moment becomes the new
definition of correct. The warning names each rule that admits no drift
(`max_delta = 0`, `equal_to_baseline`), since a rebase is the only way they
can ever be silenced. It warns rather than refuses: recording a new baseline
after a deliberate change is the command's purpose, and you still move the
paste line into `brokkr.toml` by hand, so the warning comes before anything
is rebased.

`--as-baseline` with `--gate all` is a hard error during command
validation, before a worktree or build starts; it isn't a clap parse-time
refusal:

```text
`--as-baseline` cannot be combined with `--gate all`.
```

Repinning is a per-gate decision by design. A cohort rebase re-anchors
every baseline-relative rule at once onto whatever the tree measures at that
moment, with no per-gate pause to notice a regression being blessed. Record
baselines one at a time, from a tree independently confirmed good.

## Limits

The gate doesn't do these:

- fixture-based gate auto-discovery
- a `default = "UUID"` cross-host fallback in the baseline map
- normalized metric tables or ad-hoc SQL querying
- JSON diffing of correctness against arbitrary `summary.json` shapes: the
  ratatoskr script must emit explicit scalar correctness fields (`correct =
  1`, `message_count` and so on) for the gate to compare
- automatic pruning of `gate.db` rows
