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
  git_commit    TEXT NOT NULL,      -- "dirty" alias allowed via --force
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
3. Look up that UUID in `gate.db`. If missing, fail with: `baseline UUID
   <uuid> not found in gate.db on host "<hostname>" - the pinned UUID
   was recorded on a different machine; record locally with
   --as-baseline`.
4. Validate the looked-up row's `gate_name`, `script`, and `fixture`
   match the current invocation. Mismatch is a hard error.

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

`--force` lets a dirty git tree record (rows land with `git_commit =
"dirty"`, `dirty = 1`). Dirty rows are valid baselines but flagged in the
gate report.

## Out of scope for v1

- `--gate all` and fixture-based auto-discovery.
- `default = "<uuid>"` cross-host fallback in the baseline map.
- Normalized metric tables / ad-hoc SQL querying.
- JSON-diff correctness against arbitrary `summary.json` shapes - the
  ratatoskr script must emit explicit scalar correctness fields
  (`correct = 1`, `message_count`, etc.) for the gate to compare.
- Auto-pruning of gate.db rows.
