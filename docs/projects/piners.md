# piners project notes

`project = "piners"` in `brokkr.toml`. The one command is `brokkr corpus`
(the parity-corpus runner) plus `brokkr results` (project-gated to the corpus
run store, below). Driving the runner - config, selection, verification, the
expected-disposition gate, reseed/bless - is documented in
`docs/commands/corpus.md`. This doc covers the **data contracts** the harness
emits and the **run store** brokkr persists them to. Helpers: `src/piners/`.

## The manifest hand-off

After verification brokkr writes `manifest.json` into the run dir and hands
its path to the harness, which consumes only the manifest (never re-resolving
paths or re-checking hashes). Schema:

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
`pins.toml` key) is what the harness emits - never inferred from `probe_dir`'s
basename. `expected` is brokkr-side (the gate), *not* in the manifest. The
harness ignores `pine`/`csv`/`keywords` (already verified; a record). `feeds`
are absolute, passed through verbatim.

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
- `signature`: non-exact parity probes. `dense_na_sites`: when non-empty.
- `*_fail`: carries `error` instead of the parity fields.

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
dense-na breakdown (by builtin: site/na/probe counts).

## The corpus run store (`runs.db`)

Every run's harness NDJSON is ingested into a per-project SQLite store at
`.brokkr/piners/corpus/runs.db` (gitignore it - unbounded, regenerable run
history). One transaction after the harness exits: a `run` row plus child
`disposition` / `trade_diff` / `gate_miss` / `dense_na_site` rows. Append-only
(FK clauses are declarative; enforcement off), per-db `PRAGMA user_version`
migrations, WAL - mirroring `src/db` (`ResultsDb`). Code: `src/piners/corpus_db/`.

- `run` - `started_at`, `selector` (JSON: resolved ids + raw flags), `gated`
  (`!--no-gate`), `result` (pass/fail), `fail_reason`, `harness_exit_code`,
  `probe_count`, `harness_stderr`. The exit/reason/stderr make a failed run
  self-contained.
- `disposition` (PK `run_id,probe`) - `outcome`, `disposition` (gate label),
  `expected` + `gate_ok` (from the pins at run time; `None` expected is never
  ok), `matched`/`ours_only`/`tv_only`, `count_tier`, `acc_tier`/`acc_profile`,
  `acc_failing` (JSON array), `p90_entry/exit/pnl`, `sig_domain`/`sig_leg`/
  `sig_dimension`/`sig_detail`/`sig_breaches`, `error`.
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

## Querying via `brokkr results`

piners records no benchmarks, so `results` queries `runs.db` instead of the
(empty) `results.db`. The command is bimodal: the benchmark filters
(`--commit`/`--compare`/`--command`/`--mode`/`--dataset`/`--meta`/`--env`/
`--grep`) are rejected with an error here; the corpus flags do:

- `brokkr results` - table of recent runs.
- `brokkr results <id>` / `--run <id>` - that run's per-probe dispositions (+
  gate misses + stderr). Default is the latest run.
- `brokkr results --probe <id>` - the probe's disposition + its `trade_diff`
  rows (the drill-down a blessed `actionable_drift` probe still carries).
- `brokkr results --diffs --where "<expr>"` - `trade_diff` rows across the run
  matching a raw boolean expression.
- `brokkr results --trend <probe>` - disposition/tier/p90 over recent runs.
- `brokkr results --sql "<SELECT…>"` - read-only escape hatch.

Canned views are `?N`-parameterized. `--where`/`--sql` interpolate trusted
local SQL; safety rests on the read-only DB open (the load-bearing guard), with
a `SELECT`/`WITH`-only, no-`;` UX check on top.
