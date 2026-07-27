# Ratatoskr sync-bench gate

Gated to `project = "ratatoskr"`. Layered on top of `sync-bench` (see
`docs/commands/sync.md`) to catch performance and correctness regressions
against a per-hostname pinned baseline. The gate compares scalar metrics
from the current run to a baseline row recorded in
`.brokkr/ratatoskr/gate.db` and exits non-zero on any threshold breach.

For the `[ratatoskr]` config block see `docs/brokkr.toml.md`. For the
underlying sync-bench mechanics (sæhrimnir spawn, FIFO markers, best-of-N
selection, summary.json ingestion) see `docs/commands/sync.md`.

## Storage: `.brokkr/ratatoskr/gate.db`

Committed SQLite DB (same convention as `.brokkr/results.db`). One row per
gated `sync-bench` run. Schema:

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

Every `--gate <name>` invocation of `sync-bench` writes a row, regardless
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

`brokkr sync-bench <SCRIPT> --gate <name> [--bench N] [--force] [--keep-artefacts] [--debug | --release]`

Runs sync-bench as documented in `docs/commands/sync.md`, writes a row
to gate.db, then evaluates every rule under
`[ratatoskr.gate.<name>.metrics.*]`. Reports each rule with `OK` /
`FAIL` and a numeric line; exits non-zero if any rule fails.

`--gate <name>` runs exactly one gate. Implicit discovery (`--gate all`,
or fixture-based auto-match) is out of scope for v1 - multiple gates can
reference the same script, and silent multi-gate execution is a foot-gun.

`--as-baseline` records the row as usual but suppresses gate evaluation,
prints the new UUID, and prints the exact TOML line to paste:

```
[ratatoskr.gate.jmap_small.baseline]
folk-desktop = "a344fcc2"
```

Brokkr never auto-edits `brokkr.toml`. Promotion is always a manual paste
so the diff lands in a normal commit.

`--force` lets a dirty git tree record (rows land with the real
`git_commit` SHA plus `dirty = 1`, so the SHA stays useful for
forensics). Dirty rows are valid baselines but flagged in the gate
report.

## Out of scope for v1

- `--gate all` and fixture-based auto-discovery.
- `default = "<uuid>"` cross-host fallback in the baseline map.
- Normalized metric tables / ad-hoc SQL querying.
- JSON-diff correctness against arbitrary `summary.json` shapes - the
  ratatoskr script must emit explicit scalar correctness fields
  (`correct = 1`, `message_count`, etc.) for the gate to compare.
- Auto-pruning of gate.db rows.
