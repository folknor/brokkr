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
    "inputs": { "path": "vendor/pineforge-engine/validation/<id>/inputs.json", "xxh128": "..." },
    "csv":  { "path": "vendor/pineforge-engine/validation/<id>/tv_trades.csv", "xxh128": "..." },
    "record": { "path": "vendor/pineforge-engine/validation/<id>/tv_record.json", "xxh128": "..." },
    "keywords": ["magnifier"], "feed": "eth-15m-2025",
    "bar_budget": 38000, "ohlcv_start_ms": 1700000000000, "tv_trades_csv_tz": "utc_minus_5"
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
harness ignores `pine`/`inputs`/`csv`/`record`/`keywords` (already verified;
provenance) and reads its inputs from `probe_dir` by fixed name - which is why
`inputs`, `csv` and `record` each appear only when pinned, with no version
bump. `feed`, `bar_budget`, `ohlcv_start_ms` and `tv_trades_csv_tz` are the
probe's **resolved** execution facts - `[probe_config]` prefix inheritance
already applied (`registry::resolve`), so a consumer such as piners'
measurement-manifest scripts reads them as-is and never re-derives them.
`feed` is always present: the registry lint refuses a probe resolving none,
since the harness requires the key; the other three appear only when
resolved. `feeds` holds the selection's referenced feed groups, roles
resolved absolute. Semantics and precedence against `inputs.json`:
`docs/commands/corpus.md`.

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

- `outcome`: `parity | compile_fail | runtime_fail | no_tv_data | no_overlap |
  harness_abort`. `harness_abort` is the harness supervisor's line for a probe
  whose process died or replied with no complete record set: its `error`
  carries the exit status or signal, the reason and the process's stderr
  tail. A break (exit 1), never retried, gated and blessed like
  `runtime_fail`. brokkr derives the gate label itself (the tier for
  `parity`, else the outcome) and accepts an outcome/tier pair only as a
  whole (`report::valid_label`); the line's own `disposition` field is not
  trusted.
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
  from the breakdown). The `boundary_*` <= raw invariant is a contract, not
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
- the contract kinds, each carrying `"version": 1` (below).
- any other `kind` - skipped (forward-compat).

### Contract lines and ordering (crash containment)

The harness runs as a monitor (what brokkr starts), a supervisor (all shared
setup, single-threaded) and one forked process per probe, so one dying probe
costs only its own disposition (`harness_abort`). Beside the dispositions it
emits contract lines, each with `version` 1:

- `{"kind":"setup_stage","version":1,"stage":S,"feed"?:F,"path"?:P}` before
  each shared setup step; `{"kind":"setup_complete","version":1}` when probes
  are about to run.
- `{"kind":"probe_start","version":1,"probe":ID}` once a probe process has
  bootstrapped; `{"kind":"probe_end","version":1,"probe":ID}` when it is
  settled. A probe's disposition (and its `trade_diff` lines) come between
  its start and its end. Probes overlap: these say what was in flight, never
  what caused anything.
- `{"kind":"run_error","version":1,"stage":S,"feed"?,"role"?,"path"?,"field"?,"error":E}`:
  shared setup or the harness itself failed. Names the failing entry, never
  a probe; at most once; the process exits 2.
- `{"kind":"run_end","version":1,"exit":0|1}`: last line of a completed run,
  once every probe is settled; the process exits with that value.

A run carries exactly one terminal record: `run_end` on completion, else one
`run_error` (the monitor synthesizes it when the supervisor dies without
finishing). Dispositions arrive in **completion order**, not manifest order.
brokkr's checks of all of this - version, lifecycle, terminal order, process
exit - and what each consumer accepts are in `docs/commands/corpus.md` ("The
harness contract"); a stream with no contract line at all is judged by the
older exit-and-coverage rules and reported as `no contract observed`.

brokkr parses tolerantly at the *field* level (a field it does not model is
ignored for rendering, and stored with the rest of the line - see the run
store below), but not at the *record* level: the stream must carry exactly
one valid disposition for every manifest probe and nothing for any other id,
and a stdout line that is not JSON or a disposition line that does not
deserialize is an invalid record. Any shortfall or violation fails the run
(report integrity, `docs/commands/corpus.md`), whatever the exit code. brokkr
renders per-probe lines + a computed summary (led by the reconciliation, `N
selected, M scored, K missing`) + root-cause breakdown (by `signature` domain/dimension) +
dense-na breakdown (by builtin: site/na/probe counts). When any probe carried a
window-boundary discount, a `boundary artifacts: N probes, M trades
discounted` line follows the summary (the "log what was dropped" rule - a probe
flipping `count_divergent -> accepted` on the discount would otherwise read as a
fix rather than a reclassification), and each surviving deviation line shows its
`boundary`/`effective` counts. The per-probe lines are
trimmed to the **deviations**: a probe sitting exactly on its pinned `expected`
(the gate's satisfied set) is suppressed and folded into one `N probes matching
their pin (hidden)` line, so the surviving lines are the regressions/surprise
improvements worth reading. On an unblessed corpus everything deviates, so
nothing is hidden. A `harness_abort` line is never hidden, pinned or not, and
the summary always carries `harness_abort=N`. The summary, both breakdowns
and the hidden count cover the scored set (valid dispositions of selected
probes) only, and everything is listed sorted by probe id, since
dispositions arrive in completion order.

## The corpus run store (`runs.db`)

Every run's harness NDJSON is ingested into a per-project SQLite store at
`.brokkr/piners/corpus/runs.db` (gitignore it - unbounded, regenerable run
history). One transaction after the harness exits: a `run` row plus child
`disposition` / `trade_diff` / `gate_miss` / `dense_na_site` rows. Append-only
(the bundled SQLite enforces the FK clauses; ingest writes the `run` row
first), per-db `PRAGMA user_version` migrations, WAL - mirroring `src/db`
(`ResultsDb`). Code: `src/piners/corpus_db/`.

- `run` - `run_id` (the number the run's artefact dir carries - see
  `docs/commands/corpus.md`), `started_at` (UTC, `YYYY-MM-DD HH:MM:SS`,
  taken before the build; rows older than v6 carry their ingest time
  instead), `commit_sha` + `dirty` (v6: the project checkout's full `HEAD`
  hash at run start, and whether it had any uncommitted change outside
  `.brokkr/` - markdown and `brokkr.toml` included, so clean means `HEAD`
  describes the whole invocation; a toolchain file the lock moved aside is
  judged by its moved bytes against `HEAD`'s; each `NULL` when unknown,
  never a guessed clean), `selector` (JSON: resolved ids + raw flags, forwarded
  harness flags, and the build profile as `debug`), `gated` (neither
  `--no-gate` nor `--bless`), `result` (`pass`/`fail`, or `interrupted`/
  `error` for a run that ended before its output could be ingested),
  `fail_reason`, `harness_exit_code`,
  `probe_count` (disposition lines stored, report-only extras included - not
  a coverage measure), `harness_stderr`, `wall_ms` (brokkr's own measured
  whole-run harness wall of the original attempt, never extended by
  diagnostic isolation; `NULL` for a run whose harness never finished or
  pre-v4 rows), `protocol_violations` (v7: invalid records + repeated records
  + report-only extras from the report-integrity reconciliation, plus the
  harness contract violations, plus one
  when the harness stdout was cut short and one when the run's
  `harness.stdout`/`harness.stderr` could not be written; any nonzero count bars the run from
  being runtime-ceiling evidence; `NULL` when
  the harness produced no report, or on older rows), and `diagnosis` (v7: the
  isolation pass's note - the probes that abort when run alone, the diagnosis
  status, where the attempt artefacts are - appended after the run row
  commits, beside the untouched `fail_reason`; the one write ever made to a
  stored run), and `run_error` (v8: the harness's `run_error` record stored
  whole as JSON, exactly the line the harness wrote - `kind`, `version`,
  `stage`, whichever of `feed`/`role`/`path`/`field` it carried, `error`,
  explicit nulls and any field brokkr does not model - in the run's own
  transaction; `fail_reason` is its
  readable projection, and `corpus-results <id>` renders it). The exit/reason/stderr make a failed run self-contained;
  `wall_ms` + `selector` + the disposition rows are what the pre-run runtime
  ceiling estimates the next run from (a comparable superset-covering run
  whose every selected id has a stored row whose `outcome` and `acc_tier`
  pass the shared disposition validator and agree with its `disposition`,
  and with no `harness_abort` row, which is a valid gate disposition but no
  timing sample - see `docs/commands/corpus.md`).
- `disposition` (PK `run_id,probe`) - the harness line stored **whole** as
  `raw_json`, the authoritative record, with every harness field a generated
  column projected from it. The harness adds diagnostics faster than brokkr
  learns their names, and the run dir is deleted after ingest, so a field with
  no column used to be destroyed on arrival; now it is stored the day it ships
  (reachable through `json_extract(raw_json, '$.field')` in `--sql`). Naming
  it is a projection in `schema.rs`, which reaches fresh stores, plus a
  migration step - `ALTER TABLE ... ADD COLUMN ... GENERATED ALWAYS AS (...)
  VIRTUAL` - that reaches every row already stored. The record is the parsed
  line re-serialized compactly (the harness's key order kept, its original
  bytes not). Every projection is gated on the JSON type it expects, so a
  mistyped value reads `NULL` rather than as garbage. Physical columns are only what is
  not the harness's: `disposition` (brokkr's own gate label, derived, never
  trusted from the line), `expected` (from the pins at run time; `NULL` for a
  probe outside the selection), `gate_ok` (the gate's own verdict - a probe is
  ok unless the gate flagged it, so a never-blessed selected probe is not ok
  and a stray line for an unselected probe is), and `raw_source` - `harness`,
  or `reconstructed` for a row migrated from the v4 typed table, whose record
  was rebuilt from exactly the columns it kept (so its newer diagnostics read
  `NULL`, meaning *not retained*, not zero). The projections: `outcome`,
  `matched`/`ours_only`/`tv_only`, `boundary_ours`/`boundary_tv` (the
  window-boundary discount; 0 when absent, so pre-v3 rows read as "nothing
  discounted"), `count_tier`, `acc_tier`/`acc_profile`, `acc_failing` (JSON
  array), `p90_entry/exit/pnl`, `sig_domain`/`sig_leg`/`sig_dimension`/
  `sig_detail`/`sig_breaches`, `error`, `runtime_ms` (per-probe wall-clock ms,
  surfaced by `--runtimes`), and the diagnostics: `boundary_anchor` with
  `anchor_consumed` and the per-rule split `rule_start_ours`/`rule_tail_ours`/
  `rule_start_tv`/`rule_end_tv`, `clipped_ours`/`clipped_tv`,
  `history_prefix_bars`, `oracle_trimmed`/`oracle_realtime`, the
  timestamp-shift census `ts_{entry,exit}_{considered,shifted,share_pct}` (the
  share is `NULL` when nothing was comparable, as the harness omits it),
  `window_sensitive` (JSON array) and `dynamic_builtin_calls`. The v5
  migration refuses to swap in the rebuilt table unless every projection
  reproduces the column it replaced, row for row.
  Durability covers the disposition lines the parser accepts. One it cannot
  parse is not stored as a row, but it is never silent: it is an invalid
  record, a protocol violation that fails the run under `--no-gate` too, and
  its probe counts as unscored (the report-integrity check in
  `docs/commands/corpus.md`). Its bytes survive in the stored
  `harness_stderr` only if the harness also wrote them there; the stdout
  copy goes with the run dir unless the dir is kept.
- `trade_diff` (PK `run_id,probe,our_index,tv_index`) - all 26 NDJSON fields.
  The volume driver; the PK covers probe-within-run lookups. A harness that
  repeats a disposition or `trade_diff` key has the repeats collapsed to the
  last occurrence before ingest (and the run fails - see
  `docs/commands/corpus.md`), so the PKs hold.
- `gate_miss` (PK `run_id,probe`) - selected probes the harness emitted **no**
  disposition line for (the gate violations with no disposition row).
- `dense_na_site` - one row per dense-`na` call site (`name`, `call_site`,
  `na_count`).

Because the DB is the source of truth, the run dir is dropped (pass or fail)
once ingest commits, unless `--keep-artefacts` or diagnostic isolation ran
(the `attempt-<n>/` dirs are evidence the DB holds only as the `diagnosis`
note, so the dir is preserved). Nothing a diagnostic attempt reports is
ingested. A run that ends before its
output is ingested - interrupted, a spawn failure, an unwritable manifest -
records a row under its id and keeps its dir. An ingest failure preserves the
dir and propagates. `brokkr clean` removes the `run-<id>/` dirs but spares
`runs.db`.

## Querying via `brokkr corpus-results`

The corpus run store has its own command, `brokkr corpus-results`, separate
from `brokkr results`. They used to be one: piners recorded no benchmarks, so
`results` was rerouted to `runs.db` and rejected the benchmark filters. That
broke once piners gained hotpath/alloc support - those runs land in the shared
`results.db` like every other project, so `brokkr results` keeps its benchmark
meaning and the corpus store moved to a dedicated command. No overloaded query
struct, no benchmark filters to reject. The corpus views:

- `brokkr corpus-results` - table of recent runs, each with its `commit`
  (short hash, `*` when the tree was dirty, `?` when its dirtiness is
  unknown, `-` when the commit is). The `selector` column renders
  the selection *intent* (`all` / `kw=...` / `probe=...` / `+bless`, plus
  `release` for a non-default profile and `-- <flags>` for forwarded harness
  flags), not the full
  resolved id list it stores - that would be 200+ ids wide for an `--all` run.
  The id list stays reachable via the run-detail view or `--sql`.
- `brokkr corpus-results <id>` / `--run <id>` - a `run <id>  started ... UTC
  commit ...` header, its `reason:`, its rendered `run_error` and any
  `diagnosis:` lines, then that
  run's per-probe dispositions (+ gate misses + stderr). An id with no run is
  an error. Only the **deviations**
  (rows where the stored disposition misses its pin, `gate_ok = 0`) are shown,
  plus every `harness_abort` row even when pinned - a crash is never folded
  away, as in the live rendering;
  the other pin-matchers fold into a `N probes matching their pin (hidden)` line - a
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
- `brokkr corpus-results --diffs [--probe <id>...] [--columns ...] [--where "<expr>"]` -
  the shapeable `trade_diff` table across the latest run (or `--run N`). `--probe`
  is repeatable here, an `IN`-list filter (not the combo view). `--columns
  a,b,c` projects onto a subset; `--columns all` selects every `trade_diff`
  column and renders **vertically** (psql `\x` style, since 26 columns won't fit
  a row); an unknown column name errors with the valid set - that error is the
  column-discovery path (there is no `--list-columns`). `--where` still takes a
  raw boolean expression. Default order is `(probe, our_index)`.
- `brokkr corpus-results --dispositions [--probe <id>...] [--columns ...] [--where "<expr>"]` -
  the same shaping over `disposition`, ordered by probe. The curated default
  is the window-edge diagnostics the run-detail view leaves out: the boundary
  discount, `boundary_anchor` beside `anchor_consumed` (an armed anchor that
  granted nothing is decorative), the clipped counts, each timestamp-shift
  share beside its counts (1/1 and 100/100 are both 100 percent), and
  `raw_source`. `--columns all` includes `raw_json`. `--columns`/`--where`
  without `--diffs` or `--dispositions` is an error rather than ignored, as
  is either table flag beside `--sql`, `--runtimes` or `--trend`.
- `brokkr corpus-results --runtimes [--over <secs>]` - each probe's most-recent
  runtime, slowest first, in milliseconds (the harness's unit). A **diagnostic**
  for spotting heavy probes (trim `bar_budget`, or disable), *not* the ceiling's
  basis: probes overlap in the harness, so the `sum(shown)` footer is a per-probe
  sum, several times the real run wall. The ceiling estimates from the measured
  `run.wall_ms` of a superset-covering run instead (`estimated_wall_ms`).
  `--over 269` shows what single probe nears the wall on its own.
- `brokkr corpus-results --trend <probe>` - disposition/count_tier/p90 over recent
  runs, plus the `anchor` (suffixed `+` when it granted a discount) and the
  entry/exit timestamp-shift census as `shifted/considered share%` - the
  census was added to be trended even when it does not breach. On a row
  migrated from the typed schema those cells read `n/r` (not retained).
- `brokkr corpus-results --compare <A> <B> [--full]` - two runs probe by probe,
  B read against A. Lists every probe whose `matched`/`ours_only`/`tv_only`
  counts, `count_tier`, outcome or disposition moved, as `a -> b (+delta)` -
  including count moves inside one tier, which the gate (tier against pin)
  cannot see; `--full` lists the unmoved probes too. A probe on one side only
  is classified, not dropped: `not selected` there, or `selected, no line`
  (from the stored selection and `gate_miss`); a line the harness emitted
  for a probe the run did not select is marked `(report-only)`. A line not
  carrying all three counts as numbers (a compile failure) reads `no counts`
  rather than a fall to zero, and a tier
  appearing or vanishing is a move. The `movement` column (`more divergent` /
  `less divergent` / `mixed`: matched down or either unmatched count up, the
  reverse, or both - each axis judged on its own, so one unmatched count's
  rise never cancels the other's fall) is a heuristic for comparable executions, not a verdict - a shorter
  window also lowers `matched`, and raw unmatched counts include the boundary
  artifacts the label discounts. Both run headers (start, commit) are printed,
  with a note when the runs differ in build profile or forwarded harness
  flags. Either id missing is an error. Informational: exits 0. This was the
  `--sql` query every round reached for.
- `brokkr corpus-results --sql "<SELECT...>"` - read-only escape hatch, for the genuinely
  ad-hoc query no view covers. The standing rule: when an ad-hoc query recurs,
  promote it to a named view rather than keep reaching through this door.

Canned views are `?N`-parameterized; `--columns` interpolates only allow-listed
column identifiers (a typo can't become injection). `--where`/`--sql`
interpolate trusted local SQL; safety rests on the read-only DB open (the
load-bearing guard), with a `SELECT`/`WITH`-only, no-`;` UX check on top.
