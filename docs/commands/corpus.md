# brokkr corpus: the piners parity-corpus runner

Gated to `project = "piners"`. Runs a keyword-selected slice of the
PineScript-v6 parity corpus to completion, uncapped, so the VM-iteration
loop (edit Rust, run the relevant probes, read the verdict) stays inside
the prompt-cache-warm window. `--all` is the full characterization pass,
explicitly off the fast budget. Helpers live in `src/piners/`.

## Measured runs (`--hotpath` / `--alloc`)

`corpus` is a measurable command (`docs/commands/measure.md`): the measurement
mode is a flag. A **bare** `corpus` is the parity run described in this doc
(verify → gate → `runs.db`). `corpus --hotpath [N]` / `--alloc [N]` instead
builds the `[piners.harness]` crate **with the hotpath feature added**, runs the
selection through the sidecar + hotpath-capture path, and records to
`.brokkr/results.db` - queryable with `brokkr results` like every other
project, *not* `corpus-results` (that stays the parity run store). The gate,
the runtime ceiling, and the `runs.db` ingest are all parity-only and skipped.

- Selection is the same surface (`--keyword`/`--probe`/`--all`); it builds the
  manifest workload, but no disposition is gated.
- The parity-only flags (`--verify-only`/`--reseed`/`--bless`/`--no-gate`/
  `--keep-artefacts`) conflict with the measurement flags.
- Profile defaults to **release** for measured runs (meaningful timing);
  `--debug` profiles the dev build. (Parity runs default debug.)
- `--force` is dual-purpose: ceiling-bypass in a parity run, dirty-tree in a
  measured run (the ceiling is a parity-only concept).
- `--bench` is **not** supported - the harness emits NDJSON dispositions, not
  the `key=value` stderr timing contract `--bench` consumes. Use
  `--hotpath`/`--alloc`. Code: `src/piners/measured.rs`.

## Config

The `[piners]` block (`corpus_root`, `registry_dir`, `feeds`, `harness`) is
documented in `docs/brokkr.toml.piners.md`. Kept out of the `[[check]]` sweep,
like `[ratatoskr]`; paths resolve relative to `brokkr.toml`.

## The corpus and the registry

The corpus is a read-only git submodule (default `./corpus`, upstream
`pineforge-corpus`). Each probe is a directory with `strategy.pine` (the
input) and `tv_trades.csv` (the TradingView oracle); writing inside it
diverges from upstream and is clobbered on re-pin. Probes are pinned in a
piners-owned registry (default `corpus-registry/`), two file kinds:

- `pins.toml` - the canonical, verified universe. One `[probes.<id>]`
  table per probe, pinning both files by path + xxh128 and the disposition
  the gate holds the probe to:

  ```toml
  [probes.magnifier-tick-dist-endpoints-01]
  expected = "actionable_drift"  # the blessed disposition (gate contract)
  pine = { path = "validation/<id>/strategy.pine", xxh128 = "<hex>" }
  csv  = { path = "validation/<id>/tv_trades.csv", xxh128 = "<hex>" }
  ```

  `path` is relative to `corpus_root`. `xxh128` is brokkr's standard file
  hash (`preflight::compute_xxh128`, 32 lowercase hex, case-insensitive).
  `expected` is one disposition label (see the gate, below); absent until
  the probe is blessed.

- `<keyword>.toml` (any other `*.toml`) - a pure selection grouping. The
  keyword is the file stem; body is `probes = ["id", ...]`. Ids only - the
  volatile fields (hashes, expectations) live only in `pins.toml`.

The hash is pinned, not just the name: a name alone is unverifiable because
upstream can re-pin and change a probe's bytes under the same name.

## Selection

Selection is over the pinned universe. No selection (and no `--all` /
`--verify-only`) is a hard error listing the available keywords - the slow
full-corpus pass never runs by accident.

- `--keyword <k>` (repeatable) - union of the listed groupings.
- `--probe <id>` (repeatable) - one or more probes, each resolved directly
  against `pins.toml`; the union is selected.
- `--all` - the whole pinned universe (slow characterization pass).
- `--verify-only` - verify every pinned probe against the submodule and
  exit, without building or running. Use after a submodule re-pin.
- `--reseed` - stamp `pins.toml` hashes from the corpus filesystem (below).
- `--bless` - run the selection, then stamp current dispositions (below).
- `--force` - bypass the pre-run runtime ceiling (below).

## Runtime ceiling (the pre-run wall)

After verification but before building, brokkr estimates the selection's
wall-clock cost: the sum over selected probes of each probe's **most recent
recorded `runtime_ms`** (from the corpus run store; the harness emits per-probe
runtime on each disposition line). Probes never run - or run only on harness
output predating the field - contribute 0, so a fresh DB or a never-run
selection always passes. If the estimate exceeds **270s**, the run is refused
before the build with a preflight error naming the estimate; re-run with
`--force` to override. Verification runs first, so hash/submodule drift still
surfaces on an over-budget selection. `--verify-only` is exempt (it never runs
the harness). The ceiling is a pre-run wall only - a run already underway is
never killed for exceeding it. `brokkr corpus-results --runtimes` previews the same
per-probe estimate this wall sums (see `docs/projects/piners.md`), so you can
see which probes drive the cost before a selection is refused.

## Verification (the content gate)

Each selected probe's two files are resolved under `corpus_root` and
hashed before any build. A missing path or hash mismatch is a hard error
(registry lying or submodule drifted) - no `--allow-drift`; re-stamp with
`--reseed` or fix the submodule.

## The expected-disposition gate

Aggregate floors (the old `≥132 exact` thresholds) are gone - a regression
on one probe could hide behind another's improvement. Each probe pins an
`expected` disposition, one of:

```
byte_exact | accepted | actionable_drift | count_divergent   (parity tiers)
compile_fail | runtime_fail | no_tv_data | no_overlap         (outcomes)
```

A probe's *actual* disposition is its acceptance tier (`outcome == parity`)
else the outcome. brokkr compares actual vs `expected` per selected probe;
**any** deviation fails - regression (`accepted -> count_divergent`) and
surprise improvement (`actionable_drift -> accepted`) alike, each as `id:
expected X, got Y`. No `expected` yet (freshly reseeded) is a hard "must
bless"; so is a selected probe the harness emitted no line for. `count_tier`
is *not* gated (diagnostic only). `--no-gate` downgrades the gate to
informational (still runs/aggregates/prints; harness exit governs breaks) -
for rollout or ad-hoc breakdown runs.

## Reseed and bless: the two writers of pins.toml

Independent deliberate acts, reviewed via `git diff pins.toml`: reseed
adopts new *content*, bless adopts new *dispositions*.

`--reseed` stamps hashes from the corpus **filesystem** (not `pins.toml`) -
the only way the file is created or its hashes refreshed. No build/harness.
Excludes `--verify-only`/`--keyword`.

- `--reseed --all` - stamp every parity probe under `validation/`; dirs
  without `tv_trades.csv` (self-tests, symbol containers) skipped with a
  count; vanished probes drop out.
- `--reseed --probe <id>` (repeatable) - upsert each named probe
  (hard-errors on a missing file).

Prints `added/changed/removed`. Touches `pine`/`csv` only - **preserves**
each surviving probe's `expected`; a brand-new probe stays unblessed.

`--bless [--all|--keyword <k>|--probe <id>]` runs the selection (verify +
build + harness), then stamps each probe's current disposition into
`expected`. Records reality including fails (a probe exercising an
unimplemented feature legitimately pins `expected = "compile_fail"`; the
gate then catches it starting to compile). Never gates. Prints `blessed N
(changed M)`. Excludes `--verify-only`/`--reseed`.

Bootstrap: `--reseed --all` → commit → write keyword files → `--bless
--all` → commit → runs are gated.

## Exit codes

Harness exit: `0` clean, `1` compile/runtime break(s), `2` harness error.
brokkr exits non-zero on a non-zero harness exit (or signal) **or** an
active gate deviation. Hash mismatch fails earlier (before build); the
runtime-ceiling refusal fails after verification but before the build.
`--no-gate` and `--bless` never fail on gate diffs; `--verify-only` exits 0
once all pins verify.

## Artefacts

Each invocation gets `.brokkr/piners/corpus/run-N/` holding `manifest.json`
plus captured `harness.stdout` / `harness.stderr` during the run. Every run's
NDJSON is then ingested into the corpus run store (`runs.db`), so the dir is
**always** dropped once ingest commits - unless `--keep-artefacts`, or on the
`DevError::Interrupted` / spawn-error paths. `brokkr clean` removes the
`run-N/` dirs but spares `runs.db`.

## See also

- `docs/brokkr.toml.piners.md` - the `[piners]` config block.
- `docs/projects/piners.md` - the harness NDJSON + manifest contracts, the
  `runs.db` run store and its schema, and the `brokkr corpus-results` query surface.
