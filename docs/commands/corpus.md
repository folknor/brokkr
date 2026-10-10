# brokkr corpus: the piners parity-corpus runner

Gated to `project = "piners"`. Runs a keyword-selected slice of the
PineScript-v6 parity corpus to completion, uncapped, so the VM-iteration
loop (edit Rust, run the relevant probes, read the verdict) stays inside the
prompt-cache-warm window. `--all` is the full characterization pass.
Helpers live in `src/piners/`.

## Measured runs (`--hotpath` / `--alloc`)

`corpus` is a measurable command (`docs/commands/measure.md`): the measurement
mode is a flag. A **bare** `corpus` is the parity run described in this doc
(verify -> gate -> `runs.db`). `corpus --hotpath [N]` / `--alloc [N]` instead
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
  `--debug` profiles the dev build. `[piners.harness] debug` is the parity
  default and is not consulted here. The row's `cargo_profile` records what
  was built (`dev` or `release`).
- Each iteration is bounded by the parity path's one-hour hang backstop
  (below); a harness still running then is killed and the run fails.
- Each iteration's output is read first, on every process status, and judged
  by the same run assessment as a parity run (report integrity and the
  harness contract, below). The iteration counts only if the harness exited
  0 and the run completed: every selected probe validly reported, no
  invalid, repeated or report-only extra record, no contract violation, no
  `run_error`, and the `run_end` once the contract is observed. Anything else
  fails the measurement (its timing is of a different workload), naming the
  rendered `run_error` when there is one, and printing the reconciliation,
  the in-flight context and the whole harness stderr. Exit 1 fails too, so a
  run with a `harness_abort` is never a timing sample. There is no isolation
  pass on this path. Any `harness_abort` disqualifies the iteration
  explicitly, even on exit 0 (a disposition-only stream, or a `run_end`
  saying 0). The one exemption is an iteration whose `--stop` marker
  actually fired (it ends early by design); a configured marker that never
  fired is held to the selection like any other run.
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
`strategy.pine` (input) and a TradingView oracle, at any depth and under any
tree naming (`validation/`, `strategies/`, flat). The oracle is
`tv_trades.csv` (the List of Trades export), `tv_record.json` (a tvr capture
of the in-memory strategy report at full precision), or both. When a record
is present the harness judges against it and does not read the CSV. A probe
may also carry `inputs.json` (input-panel overrides, window, timezone,
candle, syminfo block), which moves the verdict whenever present and is
pinned like the oracle.
Probes are pinned in the registry (`registry_dir`), two file kinds:

- `pins.toml` - the canonical, verified universe. `[feeds.<name>]` groups
  (hash-pinned OHLCV feeds, two forms below), `[harness_files.<name>]`
  (any other file the harness reads that moves a verdict - below),
  `[probe_config."<prefix>"]`
  (each probe's execution facts, declared by directory prefix - below),
  `[pending]` (probes registered ahead of their pins - below), and
  one `[probes.<id>]` table per probe:

  ```toml
  # single-base form: one committed 1m base feed the harness aggregates to the
  # chart TF (and uses directly as the magnifier/lower source). Its only input.
  [feeds.eth-15m-2025]
  base = { path = "vendor/pineforge-engine/data/ohlcv_ETH-USDT-USDT_1m.csv", xxh128 = "<hex>" }

  # role form (legacy): chart-TF primary plus optional warmup/lower, consumed as-is.
  [feeds.eth-15m-bench]
  primary = { path = "vendor/pineforge-benchmarks-assets/..ETHUSDT_15.csv", xxh128 = "<hex>" }
  warmup  = { path = "vendor/pineforge-benchmarks-assets/..warmup6m.csv", xxh128 = "<hex>" }

  [harness_files.probe-facts]
  path = "facts/probe-facts.toml"  # declare the path; reseed stamps xxh128
  xxh128 = "<hex>"

  [probe_config."vendor/pineforge-engine"]
  feed = "eth-15m-2025"          # the [feeds] group (oracle identity)

  [probe_config."piners"]
  feed = "eth-15m-2025"

  [probe_config."piners/some-tvr-capture"]
  feed = "eth-15m-live"          # this capture was taken on another feed
  bar_budget = 21000
  ohlcv_start_ms = 1772323200000

  [probes.magnifier-tick-dist-endpoints-01]
  expected = "actionable_drift"  # the blessed disposition (gate contract)
  pine   = { path = "vendor/pineforge-engine/validation/<id>/strategy.pine", xxh128 = "<hex>" }
  inputs = { path = "vendor/pineforge-engine/validation/<id>/inputs.json", xxh128 = "<hex>" }
  csv    = { path = "vendor/pineforge-engine/validation/<id>/tv_trades.csv", xxh128 = "<hex>" }

  [probes.some-tvr-capture]
  expected = "byte_exact"
  pine   = { path = "piners/some-tvr-capture/strategy.pine", xxh128 = "<hex>" }
  record = { path = "piners/some-tvr-capture/tv_record.json", xxh128 = "<hex>" }
  ```

  A probe pins `csv`, `record`, or both - whichever oracles its dir holds -
  plus `inputs` when its dir has an `inputs.json`. Loading refuses a pin
  with neither oracle, a probe dir that is not a plain relative path below
  `corpus_root` (the root itself, an absolute path, or a `.`/`..` component -
  reseed never discovers a probe there, and no `[probe_config]` prefix could
  cover one), and a pinned file that is not in `pine`'s directory
  under its fixed name (`strategy.pine`, `inputs.json`, `tv_trades.csv`,
  `tv_record.json`): the harness reads `<probe_dir>/<name>` and nothing else,
  so any other path would verify one file while the harness read another.
  Both writers (reseed, bless) parse their own output against these rules
  before replacing the file, so neither can write one the next load refuses.

  A feed group is either **single-base** (exactly `base`, the only committed
  input - a lower-TF feed the harness aggregates locally) or **role** (`primary`
  required, optional `warmup`/`lower`, consumed at chart TF as-is). Setting both
  `base` and `primary`, or `base` alongside `warmup`/`lower`, is a parse error.
  brokkr carries the base path+hash into the manifest (as a `base` role) and
  bumps the manifest version; all chart-TF aggregation stays harness-side.

  **`[harness_files.<name>]` - what else the harness reads.** A file outside
  the probe dirs and the feeds that still moves verdicts (a piners-owned
  facts table carrying capture origins or captured strategy properties, say)
  is pinned here by `path` + `xxh128`, under a name of the project's
  choosing. brokkr knows nothing of its contents and passes nothing new to
  the harness: the harness keeps reading the file by its own path, and the
  pin holds that file to its hash. Every run verifies **every** harness file,
  whatever the selection, since brokkr cannot tell which probes one governs.
  Without the pin such a file sits outside the content gate, the same hole an
  unpinned `inputs.json` would be. Declare an entry with `path` alone and
  reseed stamps the hash; loading refuses an entry still unstamped, or one
  whose path is not plain relative components below `corpus_root`.

  All `path`s (probe, feed and harness file) are relative to `corpus_root`. `xxh128` is
  brokkr's standard file hash (`preflight::compute_xxh128`, 32 lowercase hex,
  case-insensitive). `expected` is one disposition label (see the gate,
  below); absent until the probe is blessed. A probe entry holds nothing
  else: it is machine-stamped (reseed writes its hashes, bless its
  `expected`), and reseed drops it when the probe's directory vanishes.

  **`[probe_config."<prefix>"]` - how each probe runs.** Four optional
  fields, each flowing into the probe's manifest entry: `feed` (the `[feeds]`
  group its oracle was taken against - every probe must resolve one),
  `bar_budget` (the harness's scan cap, default 10,000), `ohlcv_start_ms`
  (the execution start) and `tv_trades_csv_tz` (the CSV oracle's timezone -
  the harness accepts `utc`, `utc_plus_N`, `utc_minus_N` or an IANA zone
  without DST, since it applies one constant offset).
  A key is a directory prefix relative to `corpus_root`, and a probe takes
  **each field independently** from the longest prefix covering its
  directory that sets it: a root declares the shared feed and budget, one
  probe dir beneath it declares only what it differs by. Coverage is by
  whole path components (`piners/a` does not cover `piners/ab`) and is
  structural - it reads the pinned and `[pending]` paths, never the
  filesystem, so a declaration over an uninitialized submodule still covers
  its probes.
  Against the probe's own `inputs.json` the precedence is the harness's and
  is per field: a resolved `ohlcv_start_ms` **outranks** an `inputs.json`
  start (a registry start corrects an upstream window guess), while an
  `inputs.json` timezone outranks a resolved `tv_trades_csv_tz`.

  These facts lived on the probe entry until a lost entry showed why they
  cannot: reseed re-added the probe with its feed re-derived from a
  directory default and its budget and start forgotten, and the probe ran
  against the wrong feed with a plausible, wrong verdict. No writer ever
  touches `[probe_config]`, so a probe entry lost and re-added comes back
  running exactly as before. The loader holds the table to rules that keep
  it honest, and since both writers parse their own output, a reseed that
  would break one is refused too:

  - a prefix is canonical (plain components joined by `/`; no `.`, `..` or
    stray slashes), and a declaration sets at least one field;
  - every declared field **governs** at least one pinned or `[pending]`
    probe - one covering none, or shadowed for every probe it covers, is
    stale (so a reseed dropping the last probe under an exact-dir
    declaration is refused until the declaration leaves in the same diff). A
    declaration written ahead of its probe is valid once the probe is
    registered in `[pending]`; unregistered, it fails every load;
  - no field **restates** the value it would inherit from an ancestor - a
    copy would silently keep the old value when the ancestor changes. An
    explicit value where no ancestor sets one is not a restatement, even if
    it equals the harness default: absent means "the harness default",
    whatever that becomes;
  - no probe with a `record` resolves a `tv_trades_csv_tz` (the harness then
    never reads the CSV, so the value is dead). There is no way to clear an
    inherited field, so declare a timezone only on prefixes covering CSV
    probes alone. A pending probe's oracle kind is unknown until it is
    pinned, so for it this rule is enforced by the reseed that pins it.

  A prefix-level `bar_budget` can move many probes in a one-line diff. That
  weakens only the review surface, not the gate: every run re-validates each
  selected probe's `expected` against what it actually did.

  **`[pending]` - probes declared before they are pinned.** Registering a
  new probe means writing its `[probe_config]` first (its feed, budget,
  start), then pinning it. A declaration that governs no probe is stale, so
  the registration names the probe ahead of its pin:

  ```toml
  [pending]
  live-02 = "piners/live-02"   # id = the directory's name
  live-03 = "piners/live-03"
  ```

  A pending probe counts for the structural rules - a declaration written
  for it governs, a keyword may list it, and the registry lint refuses one
  that would resolve no feed - but it is never selected or run, since
  nothing about it is verified: `--all` and keywords skip it with a notice,
  `--probe <id>` on it is refused, and `--verify-only` names it beside its
  OK. `--reseed --probe <id>` pins it and removes the entry in the same
  write; other pending entries are untouched, so several new probes can be
  declared together and pinned one at a time, each step a valid file. The
  loader refuses an entry no reseed could complete: a path that is not
  plain relative components or that passes through a dot-dir (discovery
  skips those), an id that is not the path's last component, an id already
  pinned, two entries on one path, and a path nested in (or around) another
  pending or pinned probe dir - discovery stops at a probe dir, so only the
  outer of two nested ones is ever found - and a path inside the registry
  dir, which discovery never walks. Whether the probe is actually on
  disk is the reseed's check, and it refuses to pin a pending probe found
  anywhere but its registered path.

- `<keyword>.toml` (any other `*.toml`) - a pure selection grouping. Keyword
  = file stem; body is `probes = ["id", ...]`. Ids only - the volatile
  fields (hashes, feeds, expectations) live only in `pins.toml`.

The hash is pinned, not just the name: a name alone is unverifiable because
upstream can re-pin and change a probe's bytes under the same name.

## Selection

Selection is over the pinned universe (`[pending]` probes are skipped, see
above). No selection (and no `--all` / `--verify-only`) is a hard error
listing the available keywords - the slow full-corpus pass never runs by
accident.

Every mode except `--reseed` takes the global brokkr lock **before** it
reads `pins.toml`, `--verify-only` included, and holds it through the run,
the ingest and any bless write. The registry's writers run under the same
lock, so the pins a run verifies are the pins it runs and blesses against:
a run that loaded and verified first, then waited out a reseed for the
lock, would otherwise have run on hashes the file no longer held. The lock
binds brokkr only - a hand edit or a git checkout during the run is not
excluded, and the harness opens live paths.

- `--keyword <k>` (repeatable or comma-separated) - union of the listed
  groupings.
- `--probe <id>` (repeatable or comma-separated, `--probe a,b,c`) - union
  of the named pinned probes.
- `--all` - the whole pinned universe (slow characterization pass).
- `--verify-only` - verify every pinned probe (and every referenced feed
  group, and every harness file) against the corpus tree and exit, without
  building or running.
  Use after a submodule re-pin.
- `--reseed` - stamp `pins.toml` hashes from the corpus filesystem (below).
- `--bless` - run the selection, then stamp current dispositions (below).
- `--force` - bypass the pre-run runtime ceiling (below).
- `--no-isolate` - skip the diagnostic isolation pass after an abnormal
  harness end (below).

## Forwarding flags to the harness

Everything after a literal `--` is appended verbatim to the harness
invocation, after `--manifest <path>`:

    brokkr corpus --probe 16-volty-expan --no-gate -- --signal-extra

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
wall-clock cost: the **measured whole-run wall** (`run.wall_ms`, brokkr's own
timing of the harness subprocess) of the most recent run whose selection was a
**superset** of the current one. Dropping probes can only shorten a run, so
`wall(subset) <= wall(superset)` makes a covering run's real wall a valid upper
bound - and any `--all` run covers everything, so one full run bounds every
selection. Only a **comparable** run is a basis: the harness exited 0 or 1 (a
break is a finished probe; exit 2, a signal, or a spawn failure is not), it
broke no report protocol or harness contract (`run.protocol_violations` is
0; a row predating that column is judged on coverage alone), **every id it
selected has a valid stored disposition** (exact coverage read from its
disposition rows, so a line for an unselected probe counts for nothing and a
line count can never stand in for a missing probe), **no probe in it is a
`harness_abort`** (a valid gate disposition, but not a usable timing sample:
the wall then describes a different workload), it ran with no forwarded
harness flags, and it
was built in the same profile as this run (a row predating the recorded
profile counts as debug). Diagnostic isolation attempts never enter the basis:
the run's `wall_ms` is the original attempt's alone. Otherwise one fast-failing
`--all` run would bound every later selection at a second or two. With no
comparable covering run recorded (a fresh DB, or a selection no prior run
superset-covers) there is no measured basis and the run proceeds. If the
estimate exceeds **270s**, the run is refused before the build with a preflight
error naming it; re-run with `--force` to override. Verification runs first, so
hash drift still surfaces on an over-budget selection; `--verify-only` is
exempt. The ceiling is a pre-run wall only - a run already underway is never
killed for exceeding it. The only mid-run limit is a one-hour **hang
backstop** far above any real run: a harness still running then is killed and
the run recorded as failed, so a wedged harness cannot hold the global lock
indefinitely. The backstop is one deadline shared by the original attempt and
any diagnostic isolation after it, counted from the original launch.

A `runs.db` written by an older brokkr is migrated to the current schema the
first time the ceiling (or `corpus-results`) reads it.

This replaced an earlier estimate that **summed** each probe's most recent
per-probe `runtime_ms`. The harness overlaps probes, so that sum ran ~5x the
real wall (a ~60s full corpus summed to ~320s), producing false refusals.
`brokkr corpus-results --runtimes` still lists per-probe runtimes (the slow-probe
"trim `bar_budget`/disable" workflow reads off it) but is a diagnostic - its
`sum(shown)` is a per-probe sum, **not** the run wall the ceiling uses.

## Verification (the content gate)

Each selected probe's pinned files (`pine`, its `csv` and/or `record`, and
`inputs` when pinned) - plus every role (`primary`/`warmup`/`lower`, or the
single `base`) of every feed group the selection references, and every
`[harness_files]` entry - are resolved under `corpus_root` and hashed before
any build. A missing path or hash
mismatch is a hard error (registry lying or the corpus drifted) - no
`--allow-drift`; re-stamp with `--reseed` or fix the tree. Every error on this
path names what it concerns - the probe, feed group (with its role) or
harness file, and the path - including the Git-LFS refusal and a file the
hasher could not read.

Two more hard errors, both about what the harness will actually do:

- A `tv_record.json` or `inputs.json` in a probe dir whose pin does not
  declare it. The harness reads both by presence alone - the record
  outranks the CSV, the inputs move the verdict - so an unpinned one would
  steer the run unverified. `--reseed --probe <id>` pins it. (An unpinned
  `tv_trades.csv` beside a record-only pin is harmless and allowed: the
  record outranks it.)
- A probe resolving no `feed`. The harness needs one per probe and would
  abort the whole run over a single feedless probe, so the registry lint
  refuses it for the whole pinned universe before any selection, naming each
  one. Declare `feed` on a `[probe_config]` prefix covering it.

**Git-LFS guard.** The `pineforge-engine` submodule routes its 1m base feed
through Git LFS, so a checkout without an LFS smudge leaves a pointer file, not
the real bytes. Before hashing *any* pinned file, brokkr sniffs its first bytes
and hard-errors on a Git-LFS pointer with a `git lfs pull` instruction naming
the owning submodule - hashing the pointer would fail verify against the real
digest, or (on `--reseed`) poison the pin with the 134-byte stub's hash. The
check is scoped and cheap (a no-op for the plaintext bench feed) and runs
*before* the runtime ceiling, so the large one-time fetch is an out-of-band
pre-warm that never counts against the 270s wall (`src/piners/lfs.rs`).

## The expected-disposition gate

Aggregate floors (the old `>=132 exact` thresholds) are gone - a regression
on one probe could hide behind another's improvement. Each probe pins an
`expected` disposition, one of:

```
byte_exact | accepted | actionable_drift | count_divergent                  (parity tiers)
compile_fail | runtime_fail | no_tv_data | no_overlap | harness_abort       (outcomes)
```

`harness_abort` is the harness's own verdict on a probe whose process died
(a signal, an abort, an OOM kill) or replied with no complete record set (see
crash containment in `docs/projects/piners.md`). It is a break like
`runtime_fail` and is pinned and blessed the same way. A crash stays visible
even when pinned: its line is always printed, never folded into `matching
their pin`, and the summary always carries a `harness_abort=N` count.

A probe's *actual* disposition is its acceptance tier (`outcome == parity`)
else the outcome. brokkr compares actual vs `expected` per selected probe;
**any** deviation fails - regression and surprise improvement alike, each as
`id: expected X, got Y`. No `expected` yet (freshly reseeded) is a hard
"must bless"; so is a selected probe the harness emitted no line for.
`count_tier` is *not* gated (diagnostic only). `--no-gate` downgrades the
gate to informational (still runs/aggregates/prints) - for rollout or ad-hoc
breakdown runs. It downgrades the gate only: the harness exit and report
integrity (below) still fail the run.

**Pinned breaks.** The harness exits 1 whenever any probe breaks
(`compile_fail`, `runtime_fail` or `harness_abort`), including a probe pinned
`expected = "compile_fail"` that is doing exactly what its pin says. brokkr therefore accepts exit 1 when every break line in the report
belongs to a selected probe pinned to that same break; an unpinned, mispinned
or unselected break still fails the run (gated or `--no-gate` alike).

**Repeated records.** The contract is one disposition line per probe and one
`trade_diff` line per `(probe, our_index, tv_index)`. A repeat is kept as its
last occurrence for display and storage, named in the output, and fails the
run (`N repeated harness records`, `1 repeated harness record` for one) as a
protocol violation (below).

## Report integrity

Independent of the gate and of `--no-gate`, the parsed report is reconciled
against the selected id set **exactly** (`src/piners/integrity.rs`), in
separate categories:

- **selected** - the probes handed to the harness;
- **scored** - selected ids with a *valid* disposition: the selected identity
  plus an outcome and tier that are valid **together** - `parity` with one of
  the four acceptance tiers, or one of the five other outcomes with no tier.
  Parseable JSON is not enough, nor is a derived label that merely looks
  pinnable: a `parity` line with no tier, `{"outcome":"accepted"}` (a tier
  posing as an outcome) and `parity` with tier `runtime_fail` are all
  invalid. The record must also agree with itself: when the harness's own
  `disposition` field is **present** it must be exactly the derived label, as
  a string - a different label, a null, a number, an object or an empty
  string makes the record invalid, and the message prints both values. Only
  a **missing** key is tolerated (a harness predating the field). Every
  occurrence is checked before repeats collapse, so a later clean repeat
  cannot hide an earlier contradiction: the probe stays unscored, never
  satisfies its pin (the gate shows `invalid (an occurrence was invalid;
  ...)`), explains no exit-1 break, and is stored with `gate_ok = 0`, so
  `corpus-results` lists it as a deviation.
  The record is still stored whole. This one record-level validator
  (`report::record_label`, over the outcome/tier primitive
  `report::valid_label`) is shared by the gate (an invalid record never
  satisfies its pin, nor explains exit 1), bless, and the runtime ceiling,
  which re-validates each stored row's `raw_json` so a contradictory record
  stored before the check is no timing evidence either;
- **missing** - selected ids with no record at all;
- **invalid** - a line for a selected id whose disposition is not a gate
  label, a disposition line that does not deserialize, or a stdout line that
  is not JSON;
- **duplicate** - repeated records (above);
- **report-only extras** - disposition lines for ids that were not selected.
  Never scored, whatever they say. An invalid record naming an unselected id
  counts as both invalid and an extra, whether it failed to parse or failed
  the cross-check.

The summary line leads with the reconciliation - `summary: N selected, M
scored, K missing` plus the invalid/duplicate/extra counts when nonzero - and
the members of each nonempty category are named on the lines after it (the
missing list capped, with a count). An empty report therefore reads as
`850 selected, 0 scored, 850 missing`, never as a bare `0 total`. The tallies
after it, the root-cause and dense-na breakdowns, and the `N probes matching
their pin (hidden)` count all cover the **scored** set only: an extra or an
invalid line is stored, but never counted as a match or aggregated.

Invalid records, duplicates and extras are **protocol violations** and fail
the run. **Any selected probe without a valid disposition fails the run**,
even on exit 0 and even under `--no-gate`: a probe that vanished is not a
probe that passed. (A `trade_diff` line that fails to parse is still only
warned about and dropped: it is a diagnostic, never scored.)

When the harness ends abnormally, or leaves selected probes without a valid
disposition, brokkr prints how it ended against how much it reported -
`harness exited 2 before reporting 850 of 850`, `harness killed by signal 11
before reporting 3 of 40` - and then the **whole** harness stderr (it is the
only evidence of an abort; it is also stored in `runs.db`). An abort after
every selected probe was validly reported is told apart: `harness exited 2
after reporting all N selected probes (an abort after complete reporting,
e.g. a teardown crash; nothing to isolate)`. Every probe is then scored, but
the exit still fails the run. A harness whose output did not close after it
exited (stdout possibly truncated) fails the run too, and the truncation is
stored in the run's `protocol_violations` count, so the run can never serve
as runtime-ceiling evidence.

If the run's own `harness.stdout`/`harness.stderr` cannot be written into its
dir, both writes are still attempted and every failed path is named. The
captured output is still parsed and ingested (`wall_ms` stays the original
attempt's), but the run fails with `evidence storage failed: ...` in its
`fail_reason`, bless is refused, the failure counts in
`protocol_violations` (barring the run from the ceiling), isolation does not
run (an explicit evidence-storage stop, recorded in the run's `diagnosis`),
and the dir is kept and reported as `artefacts preserved with incomplete
evidence (failed: <files>)`.

## The harness contract

The harness's versioned contract lines (shape in `docs/projects/piners.md`)
are checked in the same one run assessment as the reconciliation
(`src/piners/integrity.rs` builds it, `src/piners/contract.rs` validates the
contract). Every consumer reads that one assessment under its own acceptance
policy: an ordinary run and bless (the failure reasons above), a measured
iteration, an isolation attempt, and the stored ceiling evidence.

- **Recognition before version.** A line whose `kind` is one of
  `run_start`, `setup_stage`, `setup_complete`, `probe_start`, `probe_end`,
  `run_end`, `run_error` is a contract line whatever else it says (a `summary: true`
  flag included). The legacy-summary skip applies only after this, and only
  to an actual legacy summary record: `summary: true`, no `probe` key, and no
  `kind` or a kind other than `disposition`. A disposition record carrying a
  `summary` flag is still a disposition record; a missing, malformed
  or unsupported `version` (anything but integer 1) is a violation, even if
  no valid contract line appeared. Unknown kinds are still skipped.
- **No contract observed.** With no contract line at all the output says
  `no contract observed` and the run is judged by exit status and selection
  coverage alone, as before the contract existed. Once any contract line is
  seen, the rules below apply.
- **`run_start`.** Whenever any contract line appears, exactly one
  `run_start` is required and it must be **physical line 1** of stdout -
  before any disposition, summary, unknown kind or invalid line, and a
  leading blank line breaks it too. `run_start` occurrences are counted before
  version validation: a bad-version start is present (it gets its version
  defect, never `no run_start`), and a second start beside it is a
  duplicate. A stream holding `run_start` alone is contract-bearing; ended by
  a signal or by brokkr with no terminal record it is aborted/incomplete,
  while a natural exit with no terminal record is the usual violation. This
  tightens contract version 1: a contract stream without `run_start` is now
  rejected. A stream of dispositions only is still "no contract observed".
- **`run_error`** fails the run unconditionally - gated, `--no-gate` and
  bless alike - and leads the `fail_reason` as its readable projection:
  `run_error at stage <stage> (feed F, role R, path P, field X): <error>`,
  every locator the harness supplied. The original record is stored whole,
  as the harness wrote it (`kind`, `version`, explicit nulls and any field
  brokkr does not model), in the run row's `run_error` column, in the run's
  own transaction. Selected
  probes without a disposition after a `run_error` are reported as unfinished
  work (`K of N selected probes unfinished`), not as protocol violations.
  It must mean process exit 2 (unless brokkr itself killed the harness, by
  the hang backstop or an interrupt - a measured iteration's backstop kill
  is classified as such, never as a spontaneous SIGKILL); it must be the last
  line, so any record after it, an unknown kind included, is a violation;
  and a second `run_error` is a violation.
- **`run_end`** carries `exit` integer 0 or 1, appears at most once, is the
  last line of the stream, and equals the process exit. Any record after it -
  a `run_error` included - and any `run_end` beside a `run_error` is a
  violation. It claims completion, so it also requires every selected probe
  to have a `probe_start`, a disposition and a `probe_end`, in that order.
  A process that exits on its own with neither terminal record breaks the
  contract; one killed by a signal with neither is an abort, not a malformed
  stream. On any abnormal end (an unexpected exit code, a signal, the hang
  backstop) an unterminated final line that is not JSON at all - a fragment
  cut off mid-write - is reported as context, not as an invalid record. A
  final line that is complete JSON with invalid fields stays an invalid
  record, newline or not.
- **Lifecycle**, per probe, in emission order: a `probe_start` or
  `probe_end` for an unselected id, a duplicate start, a duplicate end, an
  end without a start, and a restart after an end are violations.
- **Context, never cause.** On an abnormal end or a `run_error` brokkr lists
  the probes `started, no end observed` and the last `setup_stage` seen
  (said as context only, and once `setup_complete` was seen, never as still
  running). Probes overlap, so neither names a culprit, and neither is a
  violation by itself.

Contract violations count with the reconciliation's in the stored
`protocol_violations` and fail the run (`N contract violations`). Dispositions
arrive in completion order; emission order is used for the lifecycle checks
only, and everything presented (per-probe lines, breakdown examples) is
sorted by probe id.

## Diagnostic isolation

A harness that crashes on one probe takes the whole selection with it, and
the probes overlap inside it, so the report alone cannot say which probe it
was. When the harness ends **abnormally** with selected probes unreported,
brokkr bisects them in separate harness invocations to find which abort when
run alone (`src/piners/isolate.rs`). Evidence only, never scoring.

- **Trigger.** Exit 2, any other unexpected code, or a spontaneous signal -
  *with* no `run_error` and selected probes left without a valid
  disposition. Not exit 0 or 1 (1 is the completed-with-breaks status;
  missing probes there still fail the run, as a contract breach, not a
  crash), not a `run_error` (the harness already named its own failure), not
  an interrupt or a requested shutdown, not a spawn failure, not the hang
  backstop, not an abort after complete reporting, and not when the run's own
  evidence could not be stored. `--no-isolate` disables it; the integrity
  report and the stderr print still apply. A harness with crash containment
  reports a dying probe as `harness_abort` itself, so this pass is for a
  harness whose own supervision failed.
- **Order.** The original run is recorded in `runs.db` first, and stays the
  authoritative record: its `result`, `fail_reason`, `wall_ms` (the original
  attempt's wall) and dispositions are its own. Nothing a diagnostic attempt
  reports enters the dispositions, the gate, bless or the runtime ceiling.
  Afterwards the run row's `diagnosis` gets a note - the isolated probes, the
  diagnosis status, and where the artefacts are - beside the untouched
  `fail_reason`. An interrupt during diagnosis cannot lose the original row.
- **What is bisected.** Only the original run's *unreported* selected ids
  (said in the output). Same built binary, same lock, same env, flags, cwd and
  forwarded harness args. Each attempt gets its own `attempt-<n>/` under the
  run dir, with its own manifest (built from the same verified probes),
  `harness.stdout`, `harness.stderr` and `BROKKR_HARNESS_ARTEFACT_DIR`. Each
  attempt gets the same run assessment as the original. A subset *completes*
  only if it exits 0 or 1, validly reports every probe in it (a
  `harness_abort` counts: the harness already attributed that crash), breaks
  no report protocol or contract, carries its `run_end` once the contract is
  observed, and its stdout was not cut short; a failing subset is split in
  two and each half run alone. A failure is classified, in this order:
  **protocol-invalid** (an invalid, repeated or extra record, a contract
  violation, or a cut stream - whatever the exit, so a malformed stream is
  never taken for a crash), an **abort** (exit 2, another unexpected code, a
  signal), or **incomplete** (exit 0/1, some probe without a valid
  disposition). An attempt that reports a `run_error` is none of these: it
  stops diagnosis with that harness failure, rendered, since a failure of
  the harness's own setup is nothing to bisect. If an attempt's
  `harness.stdout`/`harness.stderr` cannot be written, diagnosis stops with
  that evidence-storage failure named, rather than carry on with evidence it
  could not keep.
- **Bounds.** At most 64 attempts (every invocation counts), and one shared
  deadline - the hang backstop, counted from the original launch - of which
  each attempt gets only what remains. Diagnosis stops on an interrupt, a
  requested shutdown, the deadline, or a failure to spawn or prepare an
  attempt; a diagnostic timeout is never attributed to a probe. On the cap or
  the deadline it prints the unresolved subsets and why it stopped.
- **Wording.** It claims only what the attempts showed. A singleton that
  aborts: `probe X aborts when run alone (exit E / signal S; artefacts
  <dir>)` followed by its stderr (identical stderr from several singletons is
  printed once, naming them). A failing set both of whose halves complete:
  `abort did not reproduce in either half of {...}` (the abort needs probes
  from both halves, or is not deterministic). Every tested singleton
  aborting: `every tested singleton aborts (N)` - which is not evidence of a
  shared cause, and is not called one; it counts aborts only. Only an abort
  is ever called one, on the console and in the stored note alike: a
  singleton that exits 0/1 `reported no valid disposition when run alone`
  or `broke the report protocol when run alone`, with what was wrong, and a
  failing set that was not an abort reads `incomplete report` / `protocol
  violation did not reproduce in either half of {...}`.
- **Artefacts.** Whenever diagnosis ran, the run dir (with every
  `attempt-<n>/`) is preserved and its location printed, and the harness
  wall is printed separately from the total elapsed including diagnosis.

The crash containment proper is the harness's (a supervisor with one
process per probe, `docs/projects/piners.md`); this pass is brokkr's
evidence for when that supervision itself failed. Its wording reports exits
and signals as observed and never names a cause such as an OOM kill.

## Reseed and bless: the two writers of pins.toml

Independent deliberate acts, reviewed via `git diff pins.toml`: reseed
adopts new *content*, bless adopts new *dispositions*. Both edit the file
in place (`toml_edit`), so hand-written TOML comments survive - a comment
on a removed probe goes with it. Both hold the global brokkr lock across
their read-modify-write - bless from before the run's load to its write -
so neither can revert the other. Bless edits the text the run loaded, and
refuses (stamping nothing; the run stays recorded) if, just before it
writes, the file on disk no longer matches it: that is a hand edit made
during the run, which the write would otherwise revert. The check is not
atomic with the replace - an edit landing between the two is still
overwritten, which is the lock's limit on non-brokkr writers again.
The file is replaced atomically (temp file + rename), so a kill mid-write
leaves the old file, never a truncated one. Before the replace, each writer
parses its own output against the loader's rules, so a write can never
produce a file the next load refuses.

`--reseed` stamps hashes from the corpus **filesystem** (not `pins.toml`) -
the only way the file is created or its hashes refreshed. No build/harness.
Probe dirs are discovered anywhere under `corpus_root` by the marker (a dir
containing `strategy.pine` plus `tv_trades.csv` or `tv_record.json`, all
regular files), independent of depth and root layout; the registry dir is
excluded from the walk. The id is the dir basename - a collision across
roots is a hard error. Every oracle present is pinned, as is `inputs.json`
when present, and a file that has left the dir drops out of the pin.

- `--reseed --all` - stamp every discovered parity probe; dirs with
  `strategy.pine` but no oracle (self-tests) skipped with a count;
  vanished probes drop out. The existing file is read without the
  structural rules, so a hand edit that broke one is repaired by the regen
  instead of blocking it.
- `--reseed --probe <id>` (repeatable) - upsert each named probe. Hard-errors
  when no dir named `<id>` carries the marker, and says so specifically when
  the dir exists with `strategy.pine` but no oracle.

Either form completes the `[pending]` registrations it pins (`--all`: every
one, refusing if one is not on disk at its registered path; `--probe`: the
named ones), removing them in the same write.

Prints `added/changed/removed`. Touches the pinned *content* only:
re-hashes the probe files, the `[feeds]` group files and every
`[harness_files]` entry (`--probe` included - an edited facts file is
re-stamped by any reseed), preserves
`[probe_config]` verbatim, and carries each surviving probe's `expected`
forward. It decides nothing about how a probe runs, so a newly discovered or
re-added probe runs under whatever `[probe_config]` declares for its
directory and stays unblessed. A blessed probe that gained or lost a
`record` - so is now judged against a different oracle - keeps its
`expected` but is named in a re-bless warning, since a disposition that
happens to match across the switch would otherwise pass the gate unnoticed.
A content change that breaks a `[probe_config]` rule - the probe gained a
`record` under a declared CSV timezone, or the last probe under an exact-dir
declaration vanished - refuses the reseed, naming the declaration to edit.

`--bless [--all|--keyword <k>|--probe <id>]` runs the selection (verify +
build + harness), then stamps each probe's current disposition into
`expected`. Records reality including fails (a probe exercising an
unimplemented feature legitimately pins `expected = "compile_fail"`; the
gate then catches it starting to compile). Never gates. Prints `blessed N
(changed M)`. Excludes `--verify-only`/`--reseed`. Bless is all or nothing:
it refuses the **entire** write - before a single pin is touched, in memory
or on disk - unless the harness exit is acceptable, the report has no
protocol violation, and every selected id has a valid, stampable
disposition. Exit 1 is acceptable only when the report carries the break
lines it signals (recording them is the point); exit 2, any other code, a
signal, the hang backstop, a `run_error`, a missing or invalid record, a
repeated record, a report-only extra or a contract violation leaves
`pins.toml` untouched and exits non-zero, naming why. A `harness_abort` is
stampable like any break. The run row records `gated = no` - bless ignores the gate verdict.

Bootstrap: `--reseed --all` -> hand-write `[feeds]` groups, any
`[harness_files]` paths and the `[probe_config]` declarations -> `--reseed
--all` again (stamps feed and harness-file hashes)
-> commit -> write keyword files -> `--bless --all` -> commit -> runs are gated.

## Exit codes

Harness exit: `0` every probe scored, `1` every probe scored and at least one
break (`compile_fail`, `runtime_fail`, `harness_abort`), `2` a `run_error`.
brokkr exits non-zero on a `run_error` (always, `--no-gate` and bless
included), on a harness exit the pins do not explain - `1` with any break
not pinned to itself (see pinned breaks, above), `2`, any other code, a
signal (an abort after complete reporting included), or the hang backstop -
on a contract violation, on a report-integrity failure (a selected probe
without a valid disposition, an invalid record, a repeated record, a
report-only extra, or
output cut short), **or** on an active gate deviation. The integrity checks
apply on exit 0 and under `--no-gate` alike. Hash mismatch fails earlier
(before build); the runtime-ceiling refusal after verification but before the
build. `--no-gate` and `--bless` never fail on gate diffs; `--bless` fails
(and stamps nothing) on a failed harness or an incomplete report. An
interrupt (before or during diagnosis) exits 130 with the run recorded.
Diagnostic isolation never changes the exit: the original run's verdict
stands. `--verify-only` exits 0 once all pins (and feeds and harness files)
verify.

## Artefacts

Each run is numbered once: its dir is `.brokkr/piners/corpus/run-<id>/` and
its row in the corpus run store (`runs.db`) has the same `run_id`, so the
number a run prints (`run <id> -> <dir>`) is the one `corpus-results`
takes. The id is reserved under the lock, one past both the highest stored
run and the highest `run-<id>/` dir on disk. The dir holds `manifest.json`
plus captured `harness.stdout` / `harness.stderr`; every run's NDJSON is
ingested into `runs.db`, so the dir is dropped once ingest commits - unless
`--keep-artefacts`, or diagnostic isolation ran (its `attempt-<n>/` dirs are
not in `runs.db`, so the dir is preserved and its location printed). A run
that ends before its output can be ingested
(interrupted, the harness failed to spawn, the manifest could not be
written) is still recorded under its id (`result` `interrupted`, `fail` or
`error`) and keeps its dir. Only a run killed outright (SIGKILL, a crash) or
whose ingest itself failed leaves a dir with no row - which keeps its number
taken until `brokkr clean` removes it. Clean removes the `run-*/` dirs but
spares `runs.db`.

## See also

- `docs/brokkr.toml.piners.md` - the `[piners]` config block.
- `docs/projects/piners.md` - harness NDJSON + manifest contracts,
  `runs.db`, the `brokkr corpus-results` query surface.
