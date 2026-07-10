# piners project notes

`project = "piners"` in `brokkr.toml`. The one command is `brokkr corpus`
(the parity-corpus runner) plus `brokkr corpus-results` (its query sibling
over the corpus run store, below). Driving the runner - config, selection, verification, the
expected-disposition gate, reseed/bless - is documented in
`docs/commands/corpus.md`. This doc covers the **data contracts** the harness
emits and the **run store** brokkr persists them to. Helpers: `src/piners/`.
(`corpus --hotpath`/`--alloc` is a separate, *measured* path that records to
`.brokkr/results.db` via `brokkr results`, not the `runs.db` store here - see
the measured-runs section of `docs/commands/corpus.md`.)

## The manifest hand-off

After verification brokkr writes `manifest.json` into the run dir and hands
its path to the harness, which consumes only the manifest (never re-resolving
paths or re-checking hashes). Schema:

```json
{
  "version": 3,
  "corpus_root": "/abs/path/to/corpus",
  "probes": [{
    "probe": "<id>", "probe_dir": "vendor/pineforge-engine/validation/<id>",
    "pine": { "path": "vendor/pineforge-engine/validation/<id>/strategy.pine", "xxh128": "..." },
    "csv":  { "path": "vendor/pineforge-engine/validation/<id>/tv_trades.csv", "xxh128": "..." },
    "keywords": ["magnifier"], "feed": "eth-15m-2025",
    "bar_budget": 38000, "ohlcv_start_ms": 1700000000000, "tv_trades_csv_tz": "America/New_York"
  }],
  "feeds": {
    "eth-15m-2025":  { "base": "/abs/.../ohlcv_ETH-USDT-USDT_1m.csv" },
    "eth-15m-bench": { "primary": "/abs/...", "warmup": "/abs/...", "lower": "/abs/..." }
  }
}
```

Probe paths are relative to `corpus_root`. The explicit `probe` id (the
`pins.toml` key) is what the harness emits - never inferred from `probe_dir`'s
basename. `expected` is brokkr-side (the gate), *not* in the manifest. The
harness ignores `pine`/`csv`/`keywords` (already verified; a record). `feeds`
holds the selection's referenced feed groups, roles resolved absolute;
per-probe `feed` names a group, and the overrides appear only when pinned
(their semantics: `docs/commands/corpus.md`).

**Feed group forms (`version` 3).** A group's role map takes one of two
shapes, and the harness must branch on which. **Role form** - keys among
`primary`/`warmup`/`lower`, each already at chart TF, consumed as-is.
**Single-base form** - exactly one key, `base` (a `base` role, no `primary`):
the only committed input is a lower-TF base feed (1m OHLCV) the harness
aggregates locally to the chart-TF primary/warmup and uses directly as the
magnifier/lower source. `base` is additive, so the bump to `version = 3` is the
load-bearing signal: a harness that cannot aggregate must hard-reject on
`version != 3` rather than treat a 1m `base` as a chart-TF `primary`. brokkr
guarantees the bytes behind every feed path are materialized, not a Git-LFS
pointer (see the Git-LFS guard in `docs/commands/corpus.md`; `src/piners/lfs.rs`).

## The harness contract

Built once (`cargo build -p <pkg> --bin <bin>`, debug by default - parity is
opt-level-independent), then spawned as `<bin> --manifest
<run-dir>/manifest.json` with `BROKKR_HARNESS_ARTEFACT_DIR` (run dir) and
`BROKKR_TEST_BIN_DIR` (`target/debug/`) set, mirroring ratatoskr.

Emits **one NDJSON object per probe, no summary line** (brokkr aggregates):

```json
{"probe":"<id>","outcome":"parity","matched":218,"ours_only":0,"tv_only":0,
 "count_tier":"drift",
 "acceptance":{"tier":"actionable_drift","profile":"production","failing":["exit_price"],"p90":{"exit":0.08}},
 "signature":{"domain":"broker-fidelity","leg":"exit","dimension":"exit_price","dimension_breaches":3},
 "dense_na_sites":[{"name":"strategy.exit","call_site":"...","na_count":7}]}
```

- `outcome`: `parity | compile_fail | runtime_fail | no_tv_data | no_overlap`.
- `count_tier` (`exact|near|drift`) + `acceptance` (tier `byte_exact|accepted|
  actionable_drift|count_divergent`, profile `strict|production`, optional
  `p90{entry,exit,pnl}`): parity only.
- `ours_only`/`tv_only` (raw unmatched-pairing counts) + `boundary_ours`/
  `boundary_tv` (optional, default 0): the window-boundary-artifact discount. A
  data-start phase offset at the shared-window seam produces a burst of
  unmatched trades that vanish once both runs re-sync; piners classifies those as
  artifacts. The raw counts stay **factual** (`our_trade_count = matched +
  ours_only`, disjoint from the matched-but-divergent `trade_diff` rows), and
  `boundary_*` carries the gap. piners scores the **label and signature** on the
  *effective* divergence (`ours_only - boundary_ours`, `tv_only - boundary_tv`),
  so a boundary-only probe arrives `accepted` with `boundary_ours == ours_only`.
  brokkr persists raw + discount and renders both; it never re-nets the raw
  counts (the effective-derived signature already drops boundary-only probes
  from the breakdown). The `boundary_*` ≤ raw invariant is a contract, not
  enforced; a malformed line saturates `effective` at 0.
- `signature`: non-exact parity probes. `dense_na_sites`: when non-empty.
- `*_fail`: carries `error` instead of the parity fields.
- `runtime_ms` (optional, any outcome): per-probe wall-clock milliseconds.
  brokkr can't time probes itself (the whole selection is one harness
  subprocess), so this is the only runtime source. Persisted, and rendered by
  `brokkr corpus-results --runtimes` (below).

### Line kinds (`kind` discrimination)

Lines carry an optional `kind` field, so the harness can interleave new record
kinds without a brokkr change:

- no `kind`, or `kind == "disposition"` - the per-probe line above. The only
  kind that feeds the summary, breakdowns, and the gate. (Both forms accepted.)
- `kind == "trade_diff"` - a per-trade drill-down record, one per
  matched-but-divergent trade pair, emitted inline in probe order
  (self-limiting: an exact probe emits none). 26 fields: 9 always present
  (`probe`, `our_index`, `tv_index`, `our_entry_ts`/`our_exit_ts`,
  `our_entry_price`/`our_exit_price`, `our_qty`, `our_pnl`) + 17 nullable (the
  four `entry`/`exit` ts/price deltas, `our_entry_bar`/`our_exit_bar`,
  `our_side`/`our_entry_id`/`our_exit_id`, the `tv_*` legs incl.
  `tv_entry_qty`/`tv_pnl`/`tv_entry_signal`/`tv_exit_signal`). brokkr does not
  aggregate these but **persists** them (below).
- any other `kind` - skipped (forward-compat).

brokkr parses tolerantly (unknown fields ignored) and renders per-probe lines +
a computed summary + root-cause breakdown (by `signature` domain/dimension) +
dense-na breakdown (by builtin: site/na/probe counts). When any probe carried a
window-boundary discount, a `boundary artifacts: N probe(s), M trade(s)
discounted` line follows the summary (the "log what was dropped" rule - a probe
flipping `count_divergent -> accepted` on the discount would otherwise read as a
fix rather than a reclassification), and each surviving deviation line shows its
`boundary`/`effective` counts. The per-probe lines are
trimmed to the **deviations**: a probe sitting exactly on its pinned `expected`
(the gate's satisfied set) is suppressed and folded into one `N probe(s) match
their pin (hidden)` line, so the surviving lines are the regressions/surprise
improvements worth reading. On an unblessed corpus everything deviates, so
nothing is hidden. The summary and both breakdowns always cover the full set.

## The corpus run store (`runs.db`)

Every run's harness NDJSON is ingested into a per-project SQLite store at
`.brokkr/piners/corpus/runs.db` (gitignore it - unbounded, regenerable run
history). One transaction after the harness exits: a `run` row plus child
`disposition` / `trade_diff` / `gate_miss` / `dense_na_site` rows. Append-only
(FK clauses are declarative; enforcement off), per-db `PRAGMA user_version`
migrations, WAL - mirroring `src/db` (`ResultsDb`). Code: `src/piners/corpus_db/`.

- `run` - `started_at`, `selector` (JSON: resolved ids + raw flags), `gated`
  (`!--no-gate`), `result` (pass/fail), `fail_reason`, `harness_exit_code`,
  `probe_count`, `harness_stderr`, `wall_ms` (brokkr's own measured whole-run
  harness wall; `NULL` on a spawn failure or pre-v4 rows). The exit/reason/stderr
  make a failed run self-contained; `wall_ms` + `selector.ids` are what the
  pre-run runtime ceiling estimates the next run from (superset-covering run's
  measured wall - see `docs/commands/corpus.md`).
- `disposition` (PK `run_id,probe`) - `outcome`, `disposition` (gate label),
  `expected` + `gate_ok` (from the pins at run time; `None` expected is never
  ok), `matched`/`ours_only`/`tv_only`, `boundary_ours`/`boundary_tv` (the
  window-boundary discount; `NOT NULL DEFAULT 0`, so pre-v3 rows read as
  "nothing discounted"), `count_tier`, `acc_tier`/`acc_profile`,
  `acc_failing` (JSON array), `p90_entry/exit/pnl`, `sig_domain`/`sig_leg`/
  `sig_dimension`/`sig_detail`/`sig_breaches`, `error`, `runtime_ms` (per-probe
  wall-clock ms from the harness; absent on older output, surfaced by the
  `--runtimes` view).
- `trade_diff` (PK `run_id,probe,our_index,tv_index`) - all 26 NDJSON fields.
  The volume driver; the PK covers probe-within-run lookups.
- `gate_miss` (PK `run_id,probe`) - selected probes the harness emitted **no**
  disposition line for (the gate violations with no disposition row).
- `dense_na_site` - one row per dense-`na` call site (`name`, `call_site`,
  `na_count`).

Because the DB is the source of truth, the run dir is **always** dropped (pass
or fail) once ingest commits - only `DevError::Interrupted` and the spawn-error
path preserve it (and `--keep-artefacts`). `brokkr clean` removes the `run-N/`
dirs but spares `runs.db`. An ingest failure preserves the dir and propagates.

## Querying via `brokkr corpus-results`

The corpus run store has its own command, `brokkr corpus-results`, separate
from `brokkr results`. They used to be one: piners recorded no benchmarks, so
`results` was rerouted to `runs.db` and rejected the benchmark filters. That
broke once piners gained hotpath/alloc support - those runs land in the shared
`results.db` like every other project, so `brokkr results` keeps its benchmark
meaning and the corpus store moved to a dedicated command. No overloaded query
struct, no benchmark filters to reject. The corpus views:

- `brokkr corpus-results` - table of recent runs. The `selector` column renders
  the selection *intent* (`all` / `kw=…` / `probe=…` / `+bless`), not the full
  resolved id list it stores - that would be 200+ ids wide for an `--all` run.
  The id list stays reachable via the run-detail view or `--sql`.
- `brokkr corpus-results <id>` / `--run <id>` - that run's per-probe dispositions (+
  gate misses + stderr). Default is the latest run. Only the **deviations**
  (rows where the stored disposition misses its pin, `gate_ok = 0`) are shown;
  the pin-matchers fold into a `N probe(s) match their pin (hidden)` line - a
  200-probe `--all` run otherwise buries the few that moved. `--full` shows the
  complete table. The disposition table carries `b_ours`/`b_tv` columns (the
  window-boundary discount, `-` when none) beside raw `ours`/`tv`, so a probe
  that reads `accepted` with non-zero `ours` is self-explaining; `--trend` shows
  them too.
- `brokkr corpus-results --probe <id>` - one probe's **combo** view: its disposition +
  its `trade_diff` rows (the drill-down a blessed `actionable_drift` probe still
  carries). The curated diff columns cover all four divergence axes -
  time/price/**qty**/pnl; `our_qty`/`tv_qty` were the field the pyramiding
  investigations turned on and used to be missing. A single `--probe` only.
- `brokkr corpus-results --diffs [--probe <id>…] [--columns …] [--where "<expr>"]` -
  the shapeable `trade_diff` table across the latest run (or `--run N`). `--probe`
  is repeatable here, an `IN`-list filter (not the combo view). `--columns
  a,b,c` projects onto a subset; `--columns all` selects every `trade_diff`
  column and renders **vertically** (psql `\x` style, since 26 columns won't fit
  a row); an unknown column name errors with the valid set - that error is the
  column-discovery path (there is no `--list-columns`). `--where` still takes a
  raw boolean expression. Default order is `(probe, our_index)`.
- `brokkr corpus-results --runtimes [--over <secs>]` - each probe's most-recent
  runtime, slowest first, in milliseconds (the harness's unit). A **diagnostic**
  for spotting heavy probes (trim `bar_budget`, or disable), *not* the ceiling's
  basis: probes overlap in the harness, so the `Σ(shown)` footer is a per-probe
  sum, several times the real run wall. The ceiling estimates from the measured
  `run.wall_ms` of a superset-covering run instead (`estimated_wall_ms`).
  `--over 269` shows what single probe nears the wall on its own.
- `brokkr corpus-results --trend <probe>` - disposition/tier/p90 over recent runs.
- `brokkr corpus-results --sql "<SELECT…>"` - read-only escape hatch, for the genuinely
  ad-hoc query no view covers. The standing rule: when an ad-hoc query recurs,
  promote it to a named view rather than keep reaching through this door.

Canned views are `?N`-parameterized; `--columns` interpolates only allow-listed
column identifiers (a typo can't become injection). `--where`/`--sql`
interpolate trusted local SQL; safety rests on the read-only DB open (the
load-bearing guard), with a `SELECT`/`WITH`-only, no-`;` UX check on top.
