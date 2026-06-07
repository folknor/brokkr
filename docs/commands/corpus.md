# brokkr corpus: the piners parity-corpus runner

Gated to `project = "piners"`. Runs a keyword-selected slice of the
PineScript-v6 parity corpus to completion, uncapped, so the VM-iteration
loop (edit Rust, run the relevant probes, read the verdict) stays inside the
prompt-cache-warm window. `--all` is the full characterization pass.
Helpers live in `src/piners/`.

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
- `--bench` is **not** supported - the harness emits NDJSON dispositions,
  not the `key=value` stderr timing contract `--bench` consumes. Code:
  `src/piners/measured.rs`.

## Config

The `[piners]` block (`corpus_root`, `registry_dir`, `harness`) is documented
in `docs/brokkr.toml.piners.md`. Kept out of the `[[check]]` sweep, like
`[ratatoskr]`; paths resolve relative to `brokkr.toml`.

## The corpus and the registry

The corpus tree under `corpus_root` is piners-owned: vendor git submodules
(read-only - writing inside one diverges from upstream and is clobbered on
re-pin) plus first-party probe dirs. Each probe is a directory with
`strategy.pine` (input) and `tv_trades.csv` (the TradingView oracle), at
any depth and under any tree naming (`validation/`, `strategies/`, flat).
Probes are pinned in the registry (`registry_dir`), two file kinds:

- `pins.toml` - the canonical, verified universe. `[feeds.<name>]` groups
  (hash-pinned OHLCV feeds: `primary` plus optional `warmup`/`lower`),
  `[roots]` (root-prefix -> feed assignments, consumed by reseed), and one
  `[probes.<id>]` table per probe:

  ```toml
  [feeds.eth-15m-2025]
  primary = { path = "vendor/pineforge-engine/data/..15m.csv", xxh128 = "<hex>" }
  warmup  = { path = "vendor/pineforge-engine/data/..warmup6m.csv", xxh128 = "<hex>" }
  [roots]
  "vendor/pineforge-engine" = { feed = "eth-15m-2025" }
  [probes.magnifier-tick-dist-endpoints-01]
  expected = "actionable_drift"  # the blessed disposition (gate contract)
  feed = "eth-15m-2025"          # the [feeds] group (oracle identity)
  pine = { path = "vendor/pineforge-engine/validation/<id>/strategy.pine", xxh128 = "<hex>" }
  csv  = { path = "vendor/pineforge-engine/validation/<id>/tv_trades.csv", xxh128 = "<hex>" }
  ```

  All `path`s (probe and feed) are relative to `corpus_root`. `xxh128` is
  brokkr's standard file hash (`preflight::compute_xxh128`, 32 lowercase hex,
  case-insensitive). `expected` is one disposition label (see the gate,
  below); absent until the probe is blessed. Probes can also carry optional
  hand-edited, reseed-preserved scan overrides that flow into the manifest:
  `bar_budget` (overrides the harness's 10,000-bar scan cap; changing a
  budget changes the disposition contract, so it lives next to `expected`
  and warrants a re-bless in the same diff), `ohlcv_start_ms`, and
  `tv_trades_csv_tz` (carve-outs for vendor probes whose in-submodule
  `inputs.json` cannot carry them; probe-local `inputs.json` wins).

- `<keyword>.toml` (any other `*.toml`) - a pure selection grouping. Keyword
  = file stem; body is `probes = ["id", ...]`. Ids only - the volatile
  fields (hashes, feeds, expectations) live only in `pins.toml`.

The hash is pinned, not just the name: a name alone is unverifiable because
upstream can re-pin and change a probe's bytes under the same name.

## Selection

Selection is over the pinned universe. No selection (and no `--all` /
`--verify-only`) is a hard error listing the available keywords - the slow
full-corpus pass never runs by accident.

- `--keyword <k>` (repeatable or comma-separated) - union of the listed
  groupings.
- `--probe <id>` (repeatable or comma-separated, `--probe a,b,c`) - union
  of the named pinned probes.
- `--all` - the whole pinned universe (slow characterization pass).
- `--verify-only` - verify every pinned probe (and every referenced feed
  group) against the corpus tree and exit, without building or running.
  Use after a submodule re-pin.
- `--reseed` - stamp `pins.toml` hashes from the corpus filesystem (below).
- `--bless` - run the selection, then stamp current dispositions (below).
- `--force` - bypass the pre-run runtime ceiling (below).

## Forwarding flags to the harness

Everything after a literal `--` is appended verbatim to the harness
invocation, after `--manifest <path>`:

    brokkr corpus --probe 16-volty-expan --no-gate -- --scan-signal-extra

The allowlist-friendly replacement for env-var-prefixed invocations
(`PINERS_CORPUS_*=1 brokkr corpus ...`), whose shifting prefixes defeat
command approval. Works for parity and measured runs. Forwarded flags
are part of the run's identity: recorded in the run row's selector
(runs.db; `corpus-results` renders them as `probe=x -- --flag`) and in
`cli_args` (results.db). Conflicts with `--verify-only`/`--reseed` (no
harness runs) and `--bless` (pins must record default-behavior
dispositions only). The gate stays active - pair with `--no-gate` when
the flags change dispositions.

## Runtime ceiling (the pre-run wall)

After verification but before building, brokkr estimates the selection's
wall-clock cost: the sum over selected probes of each probe's **most recent
recorded `runtime_ms`** (from the corpus run store; the harness emits per-probe
runtime on each disposition line). Probes never run - or run only on harness
output predating the field - contribute 0, so a fresh DB or a never-run
selection always passes. If the estimate exceeds **270s**, the run is refused
before the build with a preflight error naming the estimate; re-run with
`--force` to override. Verification runs first, so hash drift still surfaces
on an over-budget selection; `--verify-only` is exempt. The ceiling is a
pre-run wall only - a run already underway is never killed for exceeding it.
`brokkr corpus-results --runtimes` previews the per-probe estimates this wall
sums - the slow-probe workflow (trim `bar_budget`, or disable) reads off it.

## Verification (the content gate)

Each selected probe's two files - plus the `primary`/`warmup`/`lower` of
every feed group the selection references - are resolved under
`corpus_root` and hashed before any build. A missing path or hash mismatch
is a hard error (registry lying or the corpus drifted) - no
`--allow-drift`; re-stamp with `--reseed` or fix the tree.

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
**any** deviation fails - regression and surprise improvement alike, each as
`id: expected X, got Y`. No `expected` yet (freshly reseeded) is a hard
"must bless"; so is a selected probe the harness emitted no line for.
`count_tier` is *not* gated (diagnostic only). `--no-gate` downgrades the
gate to informational (still runs/aggregates/prints; harness exit governs
breaks) - for rollout or ad-hoc breakdown runs.

## Reseed and bless: the two writers of pins.toml

Independent deliberate acts, reviewed via `git diff pins.toml`: reseed
adopts new *content*, bless adopts new *dispositions*. Both edit the file
in place (`toml_edit`), so hand-written TOML comments survive - a comment
on a removed probe goes with it.

`--reseed` stamps hashes from the corpus **filesystem** (not `pins.toml`) -
the only way the file is created or its hashes refreshed. No build/harness.
Probe dirs are discovered anywhere under `corpus_root` by the marker (a dir
containing `strategy.pine` + `tv_trades.csv`), independent of depth and root
layout; the registry dir is excluded from the walk. The id is the dir
basename - a collision across roots is a hard error.

- `--reseed --all` - stamp every discovered parity probe; dirs with
  `strategy.pine` but no `tv_trades.csv` (self-tests) skipped with a count;
  vanished probes drop out.
- `--reseed --probe <id>` (repeatable) - upsert each named probe (hard-errors
  when no dir named `<id>` carries both marker files).

Prints `added/changed/removed`. Touches the pinned *content* only:
re-hashes `pine`/`csv` and the `[feeds]` group files, preserves `[roots]`
verbatim, and **preserves** each surviving probe's hand-maintained fields
(`expected`, `feed`, `bar_budget`, `ohlcv_start_ms`, `tv_trades_csv_tz`).
A newly discovered probe gets `feed` assigned by the longest matching
`[roots]` prefix (an explicit `feed` always wins) and stays unblessed.

`--bless [--all|--keyword <k>|--probe <id>]` runs the selection (verify +
build + harness), then stamps each probe's current disposition into
`expected`. Records reality including fails (a probe exercising an
unimplemented feature legitimately pins `expected = "compile_fail"`; the
gate then catches it starting to compile). Never gates. Prints `blessed N
(changed M)`. Excludes `--verify-only`/`--reseed`.

Bootstrap: `--reseed --all` → hand-stamp `[feeds]`/`[roots]` + overrides →
`--reseed --all` again (stamps feed hashes, assigns feeds) → commit → write
keyword files → `--bless --all` → commit → runs are gated.

## Exit codes

Harness exit: `0` clean, `1` compile/runtime break(s), `2` harness error.
brokkr exits non-zero on a non-zero harness exit (or signal) **or** an active
gate deviation. Hash mismatch fails earlier (before build); the
runtime-ceiling refusal after verification but before the build. `--no-gate`
and `--bless` never fail on gate diffs; `--verify-only` exits 0 once all
pins (and feeds) verify.

## Artefacts

Each invocation gets `.brokkr/piners/corpus/run-N/` holding `manifest.json`
plus captured `harness.stdout` / `harness.stderr`. Every run's NDJSON is
then ingested into the corpus run store (`runs.db`), so the dir is **always**
dropped once ingest commits - unless `--keep-artefacts`, or on the
`DevError::Interrupted` / spawn-error paths. `brokkr clean` removes the
`run-N/` dirs but spares `runs.db`.

## See also

- `docs/brokkr.toml.piners.md` - the `[piners]` config block.
- `docs/projects/piners.md` - harness NDJSON + manifest contracts,
  `runs.db`, the `brokkr corpus-results` query surface.
