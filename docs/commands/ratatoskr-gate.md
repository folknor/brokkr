# Ratatoskr sync bench gate

Gated to `project = "ratatoskr"`. Layered on top of `sync --bench` (see
`docs/commands/sync.md`) to catch performance and correctness regressions
against a per-hostname pinned baseline. The gate compares scalar metrics
from the current run to a baseline row recorded in
`.brokkr/ratatoskr/gate.db` and exits non-zero on any threshold breach.

For `[ratatoskr.gate.*]` configuration see the
[ratatoskr configuration guide](../projects/ratatoskr.md#configuration).
For the underlying bench mechanics (sæhrimnir spawn, FIFO markers, best-of-N
selection, summary.json ingestion) see `docs/commands/sync.md`.

## Storage: `.brokkr/ratatoskr/gate.db`

Local SQLite DB (same convention as `.brokkr/results.db`). The gate hook
inserts a row before baseline lookup and rule evaluation, so a successful
insert remains available when comparison fails. Harness failures and other
errors before insertion produce no row. Schema:

```
CREATE TABLE gate_runs (
  uuid          TEXT PRIMARY KEY,
  created_at    INTEGER NOT NULL,   -- unix seconds
  git_commit    TEXT NOT NULL,      -- always the real short SHA; see `dirty` for tree state
  dirty         INTEGER NOT NULL,   -- 0/1
  hostname      TEXT NOT NULL,      -- libc gethostname
  gate_name     TEXT NOT NULL,      -- e.g. "jmap_small"
  script        TEXT NOT NULL,      -- project-relative path; legacy rows may be absolute
  fixture       TEXT NOT NULL,
  profile       TEXT NOT NULL,      -- dev/release
  elapsed_ms    INTEGER NOT NULL,
  exit_code     INTEGER NOT NULL,
  success       INTEGER NOT NULL,   -- 0/1
  sidecar       TEXT NOT NULL,      -- JSON blob
  meta          TEXT NOT NULL,      -- JSON blob from summary.json ingestion
  features      TEXT                -- harness build's cargo features; NULL = pre-v2 row
);
```

`features` is the `[ratatoskr.harness] features` list the harness was
built with, in canonical form: every name (entries split on `,` and
whitespace, as cargo accepts), sorted, deduplicated, joined with `,`, so
list order and shape never read as a different build. The empty string
is a default-features build. Schema v2 added the column; the v1 -> v2
migration leaves existing rows NULL rather than backfilling, because the
features an older row was built with are unknowable and a guessed value
would claim an identity it may not have.

Current rows store an in-project script path relative to the project root;
scripts outside the project remain absolute. Older rows contain canonical
absolute paths. When a checkout has moved, the gate accepts an older absolute
baseline path whose components end in the current project-relative path. New
rows store Cargo's profile name (`dev` or `release`); the legacy spelling
`debug` is read as `dev`.

Index on `(gate_name, hostname, created_at)` for the lookup paths below.
Normalized metric tables can wait until there is a real query need.

## Write policy

After all harness iterations succeed, the gate hook validates the run and
inserts its row before looking up a baseline or evaluating rules. A metric
breach or missing baseline therefore leaves the inserted row available for
inspection. A harness failure or an earlier hook error writes no row.
`--as-baseline` uses the same insert path and skips only evaluation.
Baselines are pure pointers in TOML; they don't change the write path.

If gate.db growth ever matters, prune by age or count - not in v1.

## TOML shape

```toml
[ratatoskr.gate.jmap_small]
script = "crates/app/tests/sync-harness/jmap-initial.lua"
baseline_label = "2026-05-08 jmap_small green"   # optional, human note

[ratatoskr.gate.jmap_small.baseline]
folk-desktop = "a344fcc2"
ci-linux-x64 = "81d03b7a"

[ratatoskr.gate.jmap_small.metrics.elapsed_ms]
max_relative = 1.10
max          = 5000

[ratatoskr.gate.jmap_small.metrics."sidecar.rss_peak_kb"]
max_relative = 1.15

[ratatoskr.gate.jmap_small.metrics."meta.provider_requests"]
max_delta = 0

[ratatoskr.gate.jmap_small.metrics."meta.message_count"]
equal_to_baseline = true

[ratatoskr.gate.jmap_small.metrics."meta.correct"]
equal = 1
```

`baseline.<hostname>` is the source of truth - no implicit fallback. A
later optional `default = "<uuid>"` for same-class CI workers is out of
scope for v1.

## Baseline lookup

1. Read current hostname (libc `gethostname`).
2. Look up `[ratatoskr.gate.<name>.baseline].<hostname>`. If missing,
   fail with: `no baseline pinned for host "<hostname>" in gate "<name>"
   - record one with --as-baseline and add it to brokkr.toml`.
3. Look up that UUID in `gate.db`, scoped to this hostname. The pin is a
   non-empty lowercase hexadecimal UUID or prefix of one (up to 32
   characters; `a344fcc2` is the short form below), and it must
   resolve to exactly one row: an exact match wins, a prefix matching a
   single row resolves to it, and a prefix matching several is a hard
   error naming them - never newest-wins, which would let a later run
   sharing the prefix silently become the baseline. An empty pin is
   refused, matching is byte-exact (`%` and `_` are not wildcards), and
   the run being evaluated is excluded: it is recorded before the lookup,
   and a pin that matched it would make it its own baseline, where
   `max_delta = 0` and `equal_to_baseline` pass by construction. If no
   row matches, fail - but the failure distinguishes three conditions,
   because the remedies differ and only the lookup's absence is a fact:
   - the UUID exists under **another** hostname: the pin is filed under
     the wrong host key;
   - the UUID is absent and the gate has **no other rows** on this host:
     the local `gate.db` looks reset, relocated, or newly created;
   - the UUID is absent but the gate **does** have other rows here: that
     specific row is gone, or was written to a different `gate.db`.

   In every case the message warns that `--as-baseline` is not a safe
   fix: someone pinned that UUID deliberately, and re-recording rebases
   every rule onto the current tree's numbers. On a `max_delta = 0` rule
   that silently blesses whatever regression is in the tree and leaves
   the gate permanently blind to it. Re-record only from a tree
   independently confirmed good.
4. Validate the looked-up row's `gate_name`, `script`, `fixture`,
   `profile` and `features` match the current invocation. Mismatch is a
   hard error - a `--debug` run against a release baseline (or the
   reverse) would make every timing rule meaningless, and a baseline built
   with a different feature set measured a different binary. A baseline
   whose `features` is NULL (recorded before the column existed) is
   compared anyway, with a warning that the features identity went
   unchecked: refusing it would force a re-record of every legacy pin,
   which is the blind rebase the `--as-baseline` warnings exist to
   prevent. Repin from a tree confirmed good to make the check apply.

## Bisecting with `--commit`

`sync <SCRIPT> --bench N --gate <name> --commit <ref>` evaluates the gate against a
harness built from a worktree at `<ref>`, so a breach can be walked back
to the commit that introduced it. The script and fixture deliberately do
not move with the build (see `docs/commands/sync.md`), which is what lets
the baseline's pinned `script` path keep matching across refs - the
identity check in step 4 above would otherwise hard-error on every
`--commit` run.

Because a detached worktree is clean by construction, `--commit` also
gives `--as-baseline` an unambiguous provenance: the row records the ref
that was built, and no `--force` is involved, so it never carries the
dirty tag.

## gate.db lifetime

`gate.db` lives at `<project_root>/.brokkr/ratatoskr/gate.db` - per
project, never global. It holds the only copy of every baseline's
measured numbers; `brokkr.toml` stores nothing but the UUID pointer, so
a lost row cannot be reconstructed from git.

Accordingly it has no retention or pruning policy (the store is
insert-and-select only), and `brokkr clean` does not touch it: a clean
removes the artefact and `mock/` **directories** under
`.brokkr/ratatoskr` and spares every file at that level. This is the
same carve-out piners' `runs.db` gets. Note that `brokkr kill`'s
graceful path runs `brokkr clean`, so this exemption is what keeps a
SIGTERM'd bench from destroying the baselines.

The sidecar backups under `$XDG_DATA_HOME/brokkr/sidecar-backups/` are
sidecar profile data only - unrelated to gate baselines and no recovery
path for them.

## Dirty trees

The clean-tree demand exists so the commit a baseline is pinned to
actually describes the code that produced the numbers. It does not count
`.brokkr/` against you: every store in there (`gate.db` included) is an
*output* of the run being measured, so it cannot invalidate the pin.
Nor does it count `*.md` or `brokkr.toml` - the latter mattering here
because pinning a baseline means editing `brokkr.toml`. Without those
exclusions `--as-baseline` was self-blocking, since each gated run
can write a `gate.db` row, which dirties the tree wherever a project
tracks that file: the first recording worked and every later one refused
until you committed. That is the
same trap `brokkr approve` hit with `approved.png`.

A dirty-tree run under `--force` can still reach the gate hook. If the hook
inserts its row, that row carries `dirty = 1` and the run prints a warning.
The harness's earlier `dirty tree - results will NOT be stored in database`
warning concerns `results.db`: dirty runs do not get a results row, while a
successful gate-hook insert is stored independently. A metric-rule failure
therefore remains inspectable. Harness failures and other errors before
insertion produce no gate row. Pinning a dirty row as a baseline is allowed,
and every later comparison against it re-warns.

## Rule kinds

Each metric sub-table accepts one or more of:

- `max = <scalar>`           - hard cap, current value must be `<=`
- `min = <scalar>`           - hard floor, current value must be `>=`
- `max_relative = <factor>`  - current `<=` baseline `*` factor
- `min_relative = <factor>`  - current `>=` baseline `*` factor
- `max_delta = <scalar>`     - current `-` baseline `<=` delta
- `equal = <scalar>`         - literal equality with the given scalar
- `equal_to_baseline = true` - current must equal baseline exactly

Multiple rules on the same metric all apply (logical AND). All comparisons
are scalar; no list/object diffing.

Before building the harness, brokkr compiles the configured metric names
into typed selectors and the configured rules into predicates. An unknown
bare key, an empty `sidecar.` / `meta.` key, a non-scalar `equal` value, or
a metric table with no effective predicate is rejected before build work.

Every metric sub-table must set at least one of these; a table with none
(or with only `equal_to_baseline = false`) is refused before anything is
built. It would otherwise check nothing and report PASSED. Likewise a gate
with no metric tables at all is refused unless the run is `--as-baseline`.

## Selectors

Three namespaces:

- **Bare keys** (fixed v1 set): `elapsed_ms`, `exit_code`, `success`.
  These map to top-level `gate_runs` columns. Adding bare keys is a
  schema migration. Since the gate hook runs only after successful
  iterations, `exit_code` is always `0` and `success` is always `1` for
  rows it evaluates; predicates on them cannot detect benchmark failures.
- `sidecar.<key>` - flat lookup into the sidecar JSON blob.
- `meta.<key>` - flat lookup into the `summary.json` ingestion blob.
  Numeric and boolean scalars work with numeric predicates (booleans become
  `1`/`0`); string scalars work with `equal = "..."`.

Quoted dotted keys in TOML (`"sidecar.rss_peak_kb"`) keep the namespace
prefix readable. Selector syntax is checked before build; missing keys and
null, array, or object values are errors at gate time. The sidecar projection
does emit zero for `num_threads_max`, `read_bytes`, `write_bytes`, `vol_cs`,
`nonvol_cs`, `minflt`, and `majflt` when the sidecar has no samples; other
missing selectors are not filled with a default.

## CLI

`brokkr sync <SCRIPT> --bench [N] --gate <name> [--force] [--keep-artefacts] [--debug | --release]`

`--gate` requires `--bench` - every rule compares measured numbers, so
there is nothing to evaluate on an unmeasured run.

Runs the bench as documented in `docs/commands/sync.md`; after a completed
successful bench, it writes a row to gate.db and evaluates every rule under
`[ratatoskr.gate.<name>.metrics.*]`. Reports each rule with `OK` /
`FAIL` and a numeric line; exits non-zero if any rule fails.

## `--gate all` - the cohort sweep

`--gate <name>` runs exactly one gate. The reserved value `all` runs
every configured gate in name order, each supplying its own script from
config - so the sweep takes no SCRIPT argument, and passing one is an
error rather than a silently ignored contradiction.

Shared mock and harness config, every gate script, its frontmatter and
fixture, and each rule set are validated **before anything is built**, so
a typo in a later gate surfaces immediately rather than after earlier
benches have run.

Each rule set must be non-empty and every rule must carry a predicate.

A breach does not stop the sweep. After a refactor the useful answer is
the whole blast radius, not the first provider that tripped, so every
gate runs, each failure is collected, and the command exits non-zero
with all of them listed. An interrupt is not a breach: `brokkr kill` or
Ctrl-C stops the sweep at the current gate, prints how far it got, and
exits with `DevError::Interrupted` rather than recording a FAIL and
carrying on under the lock. Fixture-based auto-match remains out of scope.

The sweep holds the global lock for its **whole duration**, not
per-gate. The per-gate bench still acquires internally (via
`BenchHarness`), but the lockfile is re-entrant within the process, so
those acquires join the sweep's hold rather than releasing between
gates. This matters for comparability against the pinned baselines:
with per-gate locking, another brokkr invocation could take the lock
between two gates and run a build or bench, contaminating the next
gate's timing - and a contaminated number still lands in gate.db and is
evaluated against the baseline, so the resulting breach would be
indistinguishable from a real regression. Holding across the sweep also
keeps the cohort contiguous in time instead of stalling mid-way behind
someone else's run.

This supersedes the v1 note that ruled `--gate all` out because "multiple
gates can reference the same script". That hazard was about *implicit*
selection - a script silently dragging in several gates. An explicit
sweep names what it runs and reports each by gate name, so two gates
sharing a script is legible rather than surprising.

### `--as-baseline` is refused with `--gate all`

Hard error during command validation, before a worktree or build starts
(this is not a clap parse-time refusal):

> `--as-baseline` cannot be combined with `--gate all`.

Repinning is a per-gate decision by design. A cohort rebase re-anchors
every baseline-relative rule at once onto whatever the tree measures at
that moment, with no per-gate pause to notice a regression being
blessed. Bare `success` and `exit_code` selectors are constants for rows
that reach the hook (`success = 1`, `exit_code = 0`), so they confirm the
recorded successful-run shape but cannot detect a harness failure. The hook
does not run after a failed bench.

Record baselines one at a time, from a tree independently confirmed
good. The single-gate `--as-baseline` path warns when it is repinning
rather than recording a first baseline; see below.

`--as-baseline` records the row as usual but suppresses gate evaluation,
prints the new UUID, and prints the exact TOML line to paste:

```
[ratatoskr.gate.jmap_small.baseline]
folk-desktop = "a344fcc2"
```

Brokkr never auto-edits `brokkr.toml`. Promotion is always a manual paste
so the diff lands in a normal commit.

When the gate *already* pins a baseline for this host, `--as-baseline`
warns first: repinning re-anchors every baseline-relative rule onto the
run in front of it, so a regression present at that moment becomes the
new definition of correct. Rules that admit no drift (`max_delta = 0`,
`equal_to_baseline`) are named individually, since a rebase is the only
way they can ever be silenced. It warns rather than refuses - recording a
new baseline after a deliberate change is the command's purpose, and the
paste-line still has to be moved into `brokkr.toml` by hand, so the
warning arrives before anything is actually rebased.

`--force` permits a gate bench on a dirty git tree. A completed gated run
lands in `gate.db` with the real `git_commit` SHA and `dirty = 1`, so the SHA
stays useful for forensics. It does not create a `results.db` row; sidecar
data remains reachable with `brokkr sidecar dirty`. Dirty gate rows are
valid baselines but flagged in the gate report.

## Out of scope for v1

- Fixture-based gate auto-discovery (`--gate all` has since landed; see
  above).
- `default = "<uuid>"` cross-host fallback in the baseline map.
- Normalized metric tables / ad-hoc SQL querying.
- JSON-diff correctness against arbitrary `summary.json` shapes - the
  ratatoskr script must emit explicit scalar correctness fields
  (`correct = 1`, `message_count`, etc.) for the gate to compare.
- Auto-pruning of gate.db rows.
