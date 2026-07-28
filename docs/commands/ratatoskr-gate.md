# Ratatoskr sync bench gate

Gated to `project = "ratatoskr"`. Layered on top of `sync --bench` (see
`docs/commands/sync.md`) to catch performance and correctness regressions
against a per-hostname pinned baseline. The gate compares scalar metrics
from the current run to a baseline row recorded in
`.brokkr/ratatoskr/gate.db` and exits non-zero on any threshold breach.

For the `[ratatoskr]` config block see `docs/brokkr.toml.md`. For the
underlying bench mechanics (sæhrimnir spawn, FIFO markers, best-of-N
selection, summary.json ingestion) see `docs/commands/sync.md`.

## Storage: `.brokkr/ratatoskr/gate.db`

Committed SQLite DB (same convention as `.brokkr/results.db`). One row per
gated `sync --bench` run. Schema:

```
CREATE TABLE gate_runs (
  uuid          TEXT PRIMARY KEY,
  created_at    INTEGER NOT NULL,   -- unix seconds
  git_commit    TEXT NOT NULL,      -- always the real short SHA; see `dirty` for tree state
  dirty         INTEGER NOT NULL,   -- 0/1
  hostname      TEXT NOT NULL,      -- libc gethostname
  gate_name     TEXT NOT NULL,      -- e.g. "jmap_small"
  script        TEXT NOT NULL,      -- absolute or repo-relative path
  fixture       TEXT NOT NULL,
  profile       TEXT NOT NULL,      -- debug/release
  elapsed_ms    INTEGER NOT NULL,
  exit_code     INTEGER NOT NULL,
  success       INTEGER NOT NULL,   -- 0/1
  sidecar       TEXT NOT NULL,      -- JSON blob
  meta          TEXT NOT NULL       -- JSON blob from summary.json ingestion
);
```

Index on `(gate_name, hostname, created_at)` for the lookup paths below.
Normalized metric tables can wait until there is a real query need.

## Write policy

Every `--gate <name>` invocation of `sync --bench` writes a row, regardless
of whether `--as-baseline` was passed. Baselines are pure pointers in
TOML; they don't change the write path. This gives local history for
free and makes baseline promotion cheap (just paste a UUID).

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
3. Look up that UUID in `gate.db`, scoped to this hostname. If missing,
   fail - but the failure distinguishes three conditions, because the
   remedies differ and only the lookup's absence is a fact:
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
4. Validate the looked-up row's `gate_name`, `script`, and `fixture`
   match the current invocation. Mismatch is a hard error.

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
writes a `gate.db` row and gate.db is tracked: the first recording
worked and every later one refused until you committed. That is the
same trap `brokkr approve` hit with `approved.png`.

A gated run under `--force` is recorded in `gate.db` with `dirty = 1`,
and the run prints a warning saying so. This does not contradict the
harness's earlier `dirty tree - results will NOT be stored in database`
warning, which governs `results.db` only: gate rows are always written
so a gate failure stays inspectable. Pinning a dirty row as a baseline
is allowed, and every later comparison against it re-warns.

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

## Selectors

Three namespaces:

- **Bare keys** (fixed v1 set): `elapsed_ms`, `exit_code`, `success`.
  These map to top-level `gate_runs` columns. Adding bare keys is a
  schema migration.
- `sidecar.<key>` - flat lookup into the sidecar JSON blob. Numeric
  scalars only.
- `meta.<key>` - flat lookup into the `summary.json` ingestion blob.
  Numeric scalars only; string equality via `equal = "..."` is allowed.

Quoted dotted keys in TOML (`"sidecar.rss_peak_kb"`) keep the namespace
prefix readable. Missing keys at gate time are a hard error - never
silently treated as zero.

## CLI

`brokkr sync <SCRIPT> --bench [N] --gate <name> [--force] [--keep-artefacts] [--debug | --release]`

`--gate` requires `--bench` - every rule compares measured numbers, so
there is nothing to evaluate on an unmeasured run.

Runs the bench as documented in `docs/commands/sync.md`, writes a row
to gate.db, then evaluates every rule under
`[ratatoskr.gate.<name>.metrics.*]`. Reports each rule with `OK` /
`FAIL` and a numeric line; exits non-zero if any rule fails.

## `--gate all` - the cohort sweep

`--gate <name>` runs exactly one gate. The reserved value `all` runs
every configured gate in name order, each supplying its own script from
config - so the sweep takes no SCRIPT argument, and passing one is an
error rather than a silently ignored contradiction.

Every gate's script is resolved and existence-checked **before anything
is built**, so a typo in the last gate surfaces immediately rather than
twenty minutes into the sweep.

A breach does not stop the sweep. After a refactor the useful answer is
the whole blast radius, not the first provider that tripped, so every
gate runs, each failure is collected, and the command exits non-zero
with all of them listed. Fixture-based auto-match remains out of scope.

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

Hard error, at parse time, before any build:

> `--as-baseline` cannot be combined with `--gate all`.

Repinning is a per-gate decision by design. A cohort rebase re-anchors
every baseline-relative rule at once onto whatever the tree measures at
that moment, with no per-gate pause to notice a regression being
blessed. The rules that make this severe are exactly the ones a sweep
would silence wholesale: on ratatoskr's own `brokkr.toml`, 14 of the 16
gates carry `max_delta = 0` and/or `equal_to_baseline`. (The two that
don't - `contacts_cadence` and `bifrost-consumer-hot-path` - express
their invariants as literal `equal = <scalar>` rules, which are
rebase-proof precisely because they aren't anchored to a baseline.)

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

`--force` lets a dirty git tree record (rows land with the real
`git_commit` SHA plus `dirty = 1`, so the SHA stays useful for
forensics). Dirty rows are valid baselines but flagged in the gate
report.

The harness's `dirty tree - results will NOT be stored in results.db`
warning governs `results.db` only. A gated run still writes to
`gate.db` - that is the write policy above, not a contradiction - and
the `recorded run <id> in gate.db` line names its database for exactly
that reason. A dirty row additionally prints a `tagged dirty` warning.

## Out of scope for v1

- Fixture-based gate auto-discovery (`--gate all` has since landed; see
  above).
- `default = "<uuid>"` cross-host fallback in the baseline map.
- Normalized metric tables / ad-hoc SQL querying.
- JSON-diff correctness against arbitrary `summary.json` shapes - the
  ratatoskr script must emit explicit scalar correctness fields
  (`correct = 1`, `message_count`, etc.) for the gate to compare.
- Auto-pruning of gate.db rows.
