# brokkr corpus: the piners parity-corpus runner

Gated to `project = "piners"`. Runs a keyword-selected slice of the
PineScript-v6 parity corpus to completion, uncapped, so the VM-iteration
loop (edit Rust, run the relevant probes, read the verdict) stays inside
the prompt-cache-warm window. `--all` is the full characterization pass,
explicitly off the fast budget. Helpers live in `src/piners/`.

## `[piners]` config

Kept out of the `[[check]]` sweep, like `[ratatoskr]`. Paths resolve
relative to `brokkr.toml`.

```toml
[piners]
corpus_root  = "corpus"          # read-only corpus submodule root (default)
registry_dir = "corpus-registry" # pins.toml + <keyword>.toml files (default)

# Shared OHLCV feeds, passed to the harness in the manifest verbatim
# (absolute after resolution). Not hash-gated.
[piners.feeds]
"1m" = "corpus/data/feed-1m.parquet"

# Corpus harness binary, reusing the shared [*.harness] shape. Required to
# run probes; --verify-only and --reseed work without it.
[piners.harness]
package = "piners-runner"  # cargo package
binary  = "corpus"         # bin (built as `cargo build -p piners-runner --bin corpus`)
# features = ["..."]       # optional
# debug = true             # corpus defaults to debug; --debug/--release also override
```

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
- `--probe <id>` - one probe, resolved directly against `pins.toml`.
- `--all` - the whole pinned universe (slow characterization pass).
- `--verify-only` - verify every pinned probe against the submodule and
  exit, without building or running. Use after a submodule re-pin.
- `--reseed` - stamp `pins.toml` hashes from the corpus filesystem (below).
- `--bless` - run the selection, then stamp current dispositions (below).

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
- `--reseed --probe <id>` - upsert one (hard-errors on a missing file).

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

## The manifest hand-off

After verification brokkr writes `manifest.json` into the run dir and hands
its path to the harness, which consumes only the manifest (never
re-resolving paths or re-checking hashes). Schema:

```json
{
  "version": 1,
  "corpus_root": "/abs/path/to/corpus",
  "probes": [{
    "probe": "<id>", "probe_dir": "validation/<id>",
    "pine": { "path": "validation/<id>/strategy.pine", "xxh128": "..." },
    "csv":  { "path": "validation/<id>/tv_trades.csv", "xxh128": "..." },
    "keywords": ["magnifier"]
  }],
  "feeds": { "1m": "/abs/path/to/feed.parquet" }
}
```

Probe paths are relative to `corpus_root`. The explicit `probe` id (the
`pins.toml` key) is what the harness emits - never inferred from
`probe_dir`'s basename. `expected` is brokkr-side (the gate), *not* in the
manifest. The harness ignores `pine`/`csv`/`keywords` (already verified; a
record). `feeds` are absolute, passed through verbatim.

## The harness contract

Built once (`cargo build -p <pkg> --bin <bin>`, debug by default - parity
is opt-level-independent), then spawned as `<bin> --manifest
<run-dir>/manifest.json` with `BROKKR_HARNESS_ARTEFACT_DIR` (run dir) and
`BROKKR_TEST_BIN_DIR` (`target/debug/`) set, mirroring ratatoskr.

Emits **one NDJSON object per probe, no summary line** (brokkr aggregates):

```json
{"probe":"<id>","outcome":"parity","matched":218,"ours_only":0,"tv_only":0,
 "count_tier":"drift",
 "acceptance":{"tier":"actionable_drift","profile":"production","failing":["exit_price"]},
 "signature":{"domain":"broker-fidelity","leg":"exit","dimension":"exit_price","dimension_breaches":3},
 "dense_na_sites":[{"name":"strategy.exit","call_site":"...","na_count":7}]}
```

- `outcome`: `parity | compile_fail | runtime_fail | no_tv_data | no_overlap`.
- `count_tier` (`exact|near|drift`) + `acceptance` (tier `byte_exact|accepted|
  actionable_drift|count_divergent`, profile `strict|production`): parity only.
- `signature`: non-exact parity probes. `dense_na_sites`: when non-empty.
- `*_fail`: carries `error` instead of the parity fields.

brokkr parses tolerantly (unknown fields ignored; `signature`/`dense_na_sites`
model only the fields it groups by) and renders per-probe lines + a computed
summary + root-cause breakdown (by `signature` domain/dimension) + dense-na
breakdown (by builtin: site/na/probe counts).

## Exit codes

Harness exit: `0` clean, `1` compile/runtime break(s), `2` harness error.
brokkr exits non-zero on a non-zero harness exit (or signal) **or** an
active gate deviation. Hash mismatch fails earlier (before build).
`--no-gate` and `--bless` never fail on gate diffs; `--verify-only` exits 0
once all pins verify.

## Artefacts

Each invocation gets `.brokkr/piners/corpus/run-N/` holding `manifest.json`
plus captured `harness.stdout` / `harness.stderr`. Dropped on success
unless `--keep-artefacts`; failures are always preserved. `brokkr clean`
wipes `.brokkr/piners/`.
