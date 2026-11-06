# brokkr corpus: the piners parity-corpus runner

Gated to `project = "piners"`. Runs a keyword-selected slice of the
PineScript-v6 parity corpus to completion, uncapped, so the VM-iteration
loop (edit Rust, run the relevant probes, read the verdict) stays inside the
prompt-cache-warm window. `--all` is the full characterization pass.
Helpers live in `src/piners/`.

## Not a measurable command

`corpus` takes no `--bench`, `--hotpath` or `--alloc`, and none of the
shared measurement flags (`--force`, `--features`, `--dry-run`, `--stop`,
`--verbose`); clap refuses them. Its own flags are the ones below, plus
`--commit REF`. The timing of a corpus run is the run row's `wall_ms` in
`runs.db` (`brokkr corpus-results`).

A measured path once existed (`--hotpath`/`--alloc` building the harness
with the hotpath feature and recording to `results.db`). It was retired
because piners retired its side: piners removed the corpus binary's hotpath
guard and counting allocator (hotpath's 64-frame ceiling aborts on deep Pine
recursion) and keeps hotpath out of the corpus build. `piners-harness`
defines no `hotpath-alloc` feature, so `--alloc` failed at cargo; `--hotpath`
ran the whole selection and then failed reading a report nothing wrote. The
corpus binary also forks a process per probe, which one report per
iteration cannot profile, and it emits no sidecar markers, so `--stop` could
never fire. `--bench` was never supported: the harness emits NDJSON
dispositions, not the `key=value` timing contract it consumes. Profiling the
corpus again needs a producer and capture contract agreed with piners first.
Rows earlier measured runs stored stay in `results.db`, readable with
`brokkr results`.

## Config

The `[piners]` block (`corpus_root`, `registry_dir`, `harness`) is documented
in `docs/brokkr.toml.piners.md`. Kept out of the `[[check]]` sweep, like
`[ratatoskr]`; paths resolve relative to `brokkr.toml`.

## Build profile

A run builds the harness under `--debug`/`--release`/`--profile
<name>` (mutually exclusive) if given, else `[piners.harness] profile`, else
`[piners.harness] debug`, else **dev**. Parity is opt-level-independent, so
the choice is a trade between build time and run time. A dev build keeps
the edit loop's rebuilds short but runs slowly, and a release build runs fast
but takes much longer to build. A custom cargo profile sits between them, for
example:

```toml
# Cargo.toml
[profile.corpus]
inherits = "dev"
opt-level = 2
```

with `profile = "corpus"` under `[piners.harness]`. It builds into
`<target>/corpus/`, so it and `brokkr check`'s dev builds never invalidate
each other's artifacts. The run row
records the profile the build used in `cargo_profile`. A run's wall is
comparable only against runs built the same way, and
`corpus-results --compare` notes a profile difference as a context mismatch.
Equal names are necessary but not sufficient, since two commits can define
the same profile differently.

## `--commit REF`

Builds the harness from a persistent worktree at `REF`, so an older engine
is judged against today's corpus. Only the harness build and the commit the
row records move: the registry, the pins and their expectations, the corpus
bytes, the manifest, the harness's working directory, `runs.db` and the
artefact dir all stay on the main tree. A parity row's `commit_sha` and `dirty` describe
the worktree the harness was built from, never the main tree, whose pins
and corpus the run used however they stood.
`--verify-only` and `--reseed` build nothing and refuse `--commit`. `--bless
--commit REF` writes `REF`'s dispositions into today's `pins.toml`, the way
to restore expectations from a known-good engine.

The build spec is the main tree's too: the package, binary, features and
profile name are chosen exactly as without `--commit` (from `[piners.harness]`
and the flags), and the worktree and its effective cargo
configuration decide what they mean - a custom profile of the same name can
be defined differently at `REF`, and an empty feature list still means that
package's defaults there. A `REF` that lacks a name the build uses fails its
build: before the harness package existed, or before the custom profile was
defined (`--profile`, `--debug` or `--release` replaces the profile for the
run). Rows record the profile, never the package or binary.

The commit's output is read by this brokkr's reader, which knows the
harness contract and the disposition vocabulary as they stand now (below, and
`docs/projects/piners.md`); brokkr keeps no reader for an earlier form. Two
earlier forms are still within `--commit`'s reach, both under contract
version 1, so the version field cannot tell them apart:

- a contract-bearing stream without `run_start`, which piners' harness wrote
  from 914b0d132 to c13d8db41 (`run_start` arrived in ac3d8f701), fails as a
  contract violation;
- a disposition carrying the retired outcome `no_tv_data` or `no_overlap`,
  which harnesses before piners 6804cfa53 could report, is an invalid record
  and fails the run, gated, `--no-gate` and `--bless --commit` alike. A run
  whose selected probes report neither is unaffected.

A stream with no contract lines at all, from a harness older than the
contract, is still judged as "no contract observed". A compatibility reader
picked by the commit's ancestry could restore the two cases above; brokkr
declines it, judging a harness only under the contract it holds today rather
than carrying another repository's protocol history.

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
  ohlcv_start_ms = 1772323200000

  [probes.magnifier-tick-dist-endpoints-01]
  expected = "actionable_drift"  # the blessed disposition (gate contract)
  pine   = { path = "vendor/pineforge-engine/validation/<id>/strategy.pine", xxh128 = "<hex>" }
  inputs = { path = "vendor/pineforge-engine/validation/<id>/inputs.json", xxh128 = "<hex>" }
  csv    = { path = "vendor/pineforge-engine/validation/<id>/tv_trades.csv", xxh128 = "<hex>" }

  [probes.some-tvr-capture]
  expected = "byte_exact"
  labels = { entry = 0, exit = 2 }  # blessed mismatch counts (the ratchets)
  performance = { equity = 0, scalars = 0 }
  excursions = { run_up = 3, drawdown = 0 }
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
  brokkr's standard file hash (`preflight::compute_file_digest`, 32 lowercase hex,
  case-insensitive). `expected` is one disposition label (see the gate,
  below); absent until the probe is blessed. `labels`, `performance` and
  `excursions` are the per-leg mismatch counts of the three ratchets
  (below): `labels` pins both `entry` and `exit`, `performance` either or
  both of `equity` and `scalars`, `excursions` either or both of `run_up` and
  `drawdown`; each is absent until a bless of a parity line carrying its
  census, and loading refuses a leg its kind does not have, an empty table,
  and a `labels` missing a leg. A pin expecting `harness_abort` also holds
  `death` and `attribution`, and one expecting `compile_fail` or
  `runtime_fail` holds `cause` (the gate, below). A probe entry holds nothing else: it is
  machine-stamped (reseed writes its hashes, bless its `expected`, a failure's
  cause, an abort's death and attribution, and its ratchet counts), and reseed drops it when the probe's directory vanishes.
  A brokkr older than a ratchet refuses a `pins.toml` holding its key.

  **`[probe_config."<prefix>"]` - how each probe runs.** Four optional
  fields, each flowing into the probe's manifest entry: `feed` (the `[feeds]`
  group its oracle was taken against - every probe must resolve one),
  `bar_budget` (the harness's scan cap on a CSV-oracle probe, default
  10,000; a record sets its own probe's right edge), the execution start
  and `tv_trades_csv_tz` (the CSV oracle's timezone - the harness accepts
  `utc`, `utc_plus_N`, `utc_minus_N` or an IANA zone without DST, since it
  applies one constant offset).

  The start has two spellings, and a declaration sets at most one:
  `ohlcv_start_ms`, a plain start, and `capture_origin_ms`, a start that is
  also the bar TradingView's chart began on - the claim the harness scores
  with its capture-origin comparison window. They are one field for every
  rule below: a probe takes the start, with its kind, from the longest prefix
  that sets either; a child restating its ancestor's value under the same
  spelling is a restatement, while a child changing only the spelling
  withdraws or asserts the origin; and an origin shadowed by plain starts for
  every probe governs nothing. An origin on a prefix claims it for every
  probe beneath it, a pending one included, and a child's plain start opts
  out. Because the claim and its value are one statement, resolution cannot
  combine an inherited start with a separately inherited origin claim;
  editing the value asserts a new measured origin, which nothing else
  checks. The manifest carries the start
  as `ohlcv_start_ms` either way, and for an origin also
  `capture_origin_ms` equal to it, in manifest version 4.
  A key is a directory prefix relative to `corpus_root`, and a probe takes
  **each field independently** from the longest prefix covering its
  directory that sets it: a root declares the shared feed, one
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
  - no probe with a `record`, a dual-oracle one included, resolves a
    `tv_trades_csv_tz` (the harness then never reads the CSV) or a
    `bar_budget` (the record sets the right edge, so the harness reads no
    budget): either value would be dead. The rule is per probe, so a
    declaration that also governs a CSV probe is still refused when it
    reaches a record probe. There is no way to clear an inherited field, so
    declare a timezone or a budget only on prefixes covering CSV probes
    alone. A pending probe's oracle kind is unknown until it is pinned, so
    for it this rule is enforced by the reseed that pins it, which names the
    declaration to edit.

  A prefix-level `bar_budget` can move many CSV probes in a one-line diff. That
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
  Use after a submodule re-pin. It cannot be combined with `--keyword` or
  `--probe`, since it always verifies the whole pinned universe.
- `--reseed` - stamp `pins.toml` hashes from the corpus filesystem (below).
- `--bless` - run the selection, then stamp current dispositions (below).
- `--no-isolate` - skip the diagnostic isolation pass after an abnormal
  harness end (below).

## Forwarding flags to the harness

Everything after a literal `--` is appended verbatim to the harness
invocation, after `--manifest <path>`:

    brokkr corpus --probe 16-volty-expan --no-gate -- --signal-extra

The allowlist-friendly replacement for env-var-prefixed invocations
(`PINERS_CORPUS_*=1 brokkr corpus ...`), whose shifting prefixes defeat
command approval. Forwarded flags
are part of the run's identity: recorded in the run row's selector and its
typed `cargo_profile`/`harness_args` context columns (runs.db;
`corpus-results` renders them as `probe=x -- --flag`). Conflicts with `--verify-only`/`--reseed` (no
harness runs) and `--bless` (pins must record default-behavior
dispositions only). The gate stays active - pair with `--no-gate` when
the flags change dispositions.

## Run length

A selection is never refused for how long it would take, and a run is let
finish. The one limit is a one-hour **hang backstop** far above any real run:
a harness still running then is killed and the run recorded as failed, so a
wedged harness cannot hold the global lock indefinitely. The backstop is one
deadline shared by the original attempt and any diagnostic isolation after it,
counted from the original launch.

A `runs.db` written by an older brokkr is migrated to the current schema before
anything is built, so a store that refuses to migrate fails the run up front
rather than at ingest.

`brokkr corpus-results --runtimes` lists each probe's most recent runtime (the
slow-probe "trim `bar_budget`/disable" workflow reads off it). Its
`sum(shown)` is a per-probe sum, **not** the run wall: the harness overlaps
probes, so the sum runs several times the real wall (`run.wall_ms`, brokkr's
own timing of the harness subprocess).

## Probe scheduling

The harness fills its job slots from the front of the manifest, so brokkr
orders the manifest slowest first by estimated scan duration. A probe's
estimate is its `runtime_ms` from the latest run that qualifies: same build
profile and forwarded harness flags as this run, the probe in that run's
selection (not a report-only extra), not marked unscored (`scored = 0`: an
invalid record or a tainted probe), not a `harness_abort` (which times the
failure, not the workload), and no dropped disposition line naming it in
that run's `report_warnings`. A historical run whose selector has no `ids`
array, or whose `report_warnings` is not a JSON array, contributes no
estimates. Each ingest folds its run into the `schedule_estimate` table, so
reading the estimates is a keyed lookup rather than a pass over history, and
an estimate outlives the retention that prunes its run. A probe with no estimate is placed at the upper
median of the selection's estimates; on equal estimates a recorded one goes
first, otherwise ties keep selection order. With no estimates at all, or a
store that cannot be read (a warning), the run keeps selection order.

The estimate is advisory: `runtime_ms` is the harness's scan time, not how
long the probe holds its slot, and a changed workload is corrected only by
its next qualifying run. A wrong estimate costs wall time, never a verdict.
The order is chosen once, before the manifest is written; diagnostic
isolation attempts keep it within each subset, and the run's `selector`
records it as `execution_order`.

## Verification (the content gate)

Each selected probe's pinned files (`pine`, its `csv` and/or `record`, and
`inputs` when pinned) - plus every role (`primary`/`warmup`/`lower`, or the
single `base`) of every feed group the selection references, and every
`[harness_files]` entry - are resolved under `corpus_root` and hashed before
any build. Hashing goes through brokkr's stamp-keyed hash cache, loaded once
and saved once for the whole selection, so an unchanged file costs a stat.
A missing path or hash
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
before anything is built, so the large one-time fetch is an out-of-band
pre-warm, never part of a run (`src/piners/lfs.rs`).

## The expected-disposition gate

Aggregate floors (the old `>=132 exact` thresholds) are gone - a regression
on one probe could hide behind another's improvement. Each probe pins an
`expected` disposition, one of:

```
byte_exact | accepted | actionable_drift | count_divergent                  (parity tiers)
compile_fail | runtime_fail | harness_abort                                 (outcomes)
```

A pinned `compile_fail` or `runtime_fail` holds **the cause it was blessed
on**, the line's closed `cause`:

```toml
[probes.deep-loop-01]
expected = "runtime_fail"
cause = "budget"
```

`cause` is one of `script`, `capability`, `budget` or `interpreter_fault`,
and on `runtime_fail` alone `panic`, an engine panic caught at the probe
process's outer boundary. Bless stamps it and the gate compares it with the
label, so a pinned typed stop that turns into a panic deviates, `expected
runtime_fail (budget), got runtime_fail (panic)`, and leaves exit 1
unexplained. A cause is a classification, not an identity: one script
refusal replacing another keeps its cause and passes. `pins.toml` refuses
`panic` on a `compile_fail` and a `cause` on any other pin. A failure pin
without a `cause`, blessed before piners sent one, still loads but reads as
unblessed: the gate fails it as `not blessed`, and a `--bless` adopts the
cause its line reports.

`harness_abort` is the harness's own verdict on a probe whose process died
(a signal, an abort, an OOM kill) or replied with no complete record set (see
crash containment in `docs/projects/piners.md`). An engine panic is not one:
piners reports it as `runtime_fail` with cause `panic`. What is left is the residue the harness
cannot attribute - it cannot say whether the engine or the harness died - so
a pin holds an abort by **the death it was blessed on**, plus a human
**attribution**:

```toml
[probes.deep-recursion-01]
expected = "harness_abort"
death = { kind = "signal", number = 6 }
attribution = "engine recursion overflows the stack on this script"
```

`death` is the line's own closed `death` (`signal` with its `number`, `exit`
with its nonzero `code`, or `reply_failure`), stamped by bless, and the gate
compares it with the label: a pinned abort on signal 6 that dies on signal 9
deviates, `expected harness_abort (signal 6), got harness_abort (signal 9)`,
and leaves exit 1 unexplained, gated or `--no-gate`. The attribution is the
human's statement the pin was accepted with. It is not compared: the gate
holds the death class, not the explanation, so a fixed defect replaced by
another with the same death passes, and `reply_failure` covers the widest
range of causes. `pins.toml` refuses a `death` or `attribution` on a pin not
expecting `harness_abort`, and an abort pin missing either, or with an
attribution that is not one nonblank line. A crash stays visible even when
pinned: its line is always printed with its death, never folded into
`matching their pin`, and the summary always carries a `harness_abort=N`
count.

**`unevaluated`** is the harness's line for a probe whose experiment could
not be conducted: the oracle does not read, holds no comparable round, fails
its quantity witness contract, reaches past the executed feed, or never meets
it; the probe's setup cannot run the experiment; two provenance facts
contradict each other; or the harness broke its own invariant while scoring
(`reason`, below). It is never a pass, so it is never pinned: `pins.toml`
refuses `expected = "unevaluated"` at load, and bless refuses its whole write
when any selected probe reports one. Every `unevaluated` probe fails the run
unconditionally - gated, `--no-gate` and bless alike - as `N unevaluated
probes (id (reason), ...)`, on its own path rather than through the gate, and
the exit-1 acceptance for pinned breaks below never covers it. Its line is
always printed, with its reason and detail, and the summary always carries an
`unevaluated=N` count.

`no_tv_data` and `no_overlap` are retired: both passed their pin inertly
whatever the engine did, and piners now reports those cases as
`unevaluated`. A new harness line carrying either is an invalid record, and a
pin expecting either is refused at load. Runs stored before the change still
display them.

A probe's *actual* disposition is its acceptance tier (`outcome == parity`)
else the outcome. brokkr compares actual vs `expected` per selected probe;
**any** deviation fails - regression and surprise improvement alike, each as
`id: expected X, got Y`. No `expected` yet (freshly reseeded) is a hard
"must bless"; so is a selected probe the harness emitted no line for.
`count_tier` is *not* gated (diagnostic only). `--no-gate` downgrades the
gate to informational (still runs/aggregates/prints) - for rollout or ad-hoc
breakdown runs. It downgrades the gate only: the harness exit and report
integrity (below) still fail the run.

**The ratchets.** Beside its tier, a parity line carries per-pair agreement
censuses that never move the tier, and each is ratcheted against counts
blessed beside `expected`:

- `labels` (every parity line): displayed-label agreement over the matched
  pairs, legs `entry` and `exit`, each `{compared, mismatched, ambiguous,
  unavailable}` (piners `reference/corpus.md`, Label comparison). A label
  regression on a `byte_exact` probe would otherwise pass the disposition
  check, and the ratchet needs no judgement of which probes' labels should
  agree - a probe whose labels legitimately differ (counter-built ids,
  value-bearing comments) pins its count.
- `performance` (a record probe the harness checked `strategy.equity` per
  bar or the report scalars for: max drawdown and max run-up, in cash and in
  percent): legs `equity` and `scalars`, each `{compared, mismatched,
  tolerance}`, either or both present. Equity can drift while every trade
  matches, and a legitimate drift pins its count.
- `excursions` (a parity line whose oracle states a comparable per-trade
  run-up or drawdown): legs `run_up` and `drawdown`, each `{compared,
  mismatched, ambiguous, unavailable, tolerance}`, a leg present exactly when
  eligible, zero compared included (piners `reference/corpus.md`, Excursion
  comparison). An excursion is the trade's accounting over its path, not what
  the trade was, so an excursion regression on a probe whose tier holds
  would otherwise pass.

The tier keeps meaning "the trades match". One rule holds for every kind,
leg by leg. Bless pins each carried leg's `mismatched` count (`labels =
{ entry = 0, exit = 2 }`, `performance = { equity = 0, scalars = 1 }`,
`excursions = { run_up = 3, drawdown = 0 }`), and the gate fails a selected
probe whose pinned count **moved** either way, as `id: excursions run_up
mismatches rose 3 -> 5 of 412 compared, 0 ambiguous (not gated) (tolerance
float32 within 1e-9 dust)` or `... fell 5 -> 2 (improvement - re-bless to
adopt) ...`: a fall fails like a tier improvement, so the pin tightens rather
than leaving the old allowance for a later regression. `ambiguous`, where a
census counts it, moves with the pairing and is shown, never gated;
`tolerance`, where a leg carries one, is piners' own label for what counts as
a mismatch, stored and shown, never interpreted - the comparison is the
harness's.

- Only a pinned leg is gated. A pin without the key, or a leg the line
  carries that the pin lacks, is ungated until a bless stamps it, so adopting
  a ratchet changes no verdict. Each kind prints a coverage line, `labels: N
  probes ratcheted, M with an unpinned leg (ungated until blessed), K
  ambiguous (not gated)` (the ambiguous count only for the kinds that tally
  it, summed over every checked census, pinned or not): a probe is ratcheted
  when it has a pinned leg its line carries, and awaits a stamp when its line
  carries a leg its pin lacks - for `labels`, which every parity line
  carries, also when an unpinned parity line carries no census (a harness
  predating it). A probe whose line carries no such census and whose pin
  holds none is in neither count.
- A pinned leg the line does not carry fails as missing evidence (`id:
  labels entry pinned at 0 mismatches, but the line carries no labels entry
  census`) - a parity line that lost a leg, or a non-parity line - unless the
  disposition check already deviates. Only the disposition check excuses it:
  one ratchet's deviation never excuses another's missing leg, and nothing
  excuses a moved count.
- Bless stamps every leg a parity line carries, clears the kind on a
  non-parity line, and refuses to drop a pinned leg the parity line does not
  carry (`a pinned excursions leg the parity line does not carry for id
  (run_up)`).
- A present census is gate evidence and is read strictly; anything else
  makes the line an invalid record. It must be an object; each present leg an
  object (a `null` leg is invalid) with every count its kind carries a
  non-negative integer, `mismatched <= compared`, and where `ambiguous` is
  counted `mismatched + ambiguous <= compared`; `tolerance`, where the kind
  carries one, a nonempty single-line string. `labels` needs both legs, the
  others at least one. Unknown keys are tolerated.
- The `performance` `scalars` leg also carries `pairs`, its figures as
  `{figure, ours, theirs, match}` (`brokkr corpus-results --scalars` lists
  them). When present they must agree with the leg: an array of objects, each
  a nonempty `figure` no other pair names, `ours` and `theirs` a number or one
  of the metric spellings `inf`, `-inf`, `nan`, a boolean `match`; one pair
  per comparison (`compared`), and `mismatched` exactly the pairs whose
  `match` is false. This checks structure and the count only - which figures
  piners compares, and whether a pair should match, are its policy. A line
  without `pairs` (a harness predating them) is valid.
- Only a valid, untainted line is checked.

**Pinned breaks.** The harness exits 1 whenever any probe breaks
(`compile_fail`, `runtime_fail`, `harness_abort` or `unevaluated`), including a probe pinned
`expected = "compile_fail"` that is doing exactly what its pin says. brokkr therefore accepts exit 1 when every break line in the report
belongs to a selected probe pinned to that same break - an abort to that same
death, a failure to that same cause, the comparison the gate makes; an unpinned, mispinned
or unselected break still fails the run (gated or `--no-gate` alike), and so
does an `unevaluated` line, which no pin can hold.

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
  the four acceptance tiers, or one of the four other live outcomes
  (`compile_fail`, `runtime_fail`, `harness_abort`, `unevaluated`) with no
  tier. Parseable JSON is not enough, nor is a derived label that merely looks
  pinnable: a `parity` line with no tier, `{"outcome":"accepted"}` (a tier
  posing as an outcome), `parity` with tier `runtime_fail` and a retired
  `no_tv_data` or `no_overlap` are all invalid. An `unevaluated` line must
  also carry a `reason` - one of `invalid_evidence`, `insufficient_evidence`,
  `witness_contract`, `feed_coverage`, `no_overlap`, `invalid_setup`,
  `conflicting_provenance`, `harness_fault` - and a nonempty one-line
  `error`, and its shape is closed: it carries no key but `kind`, `probe`,
  `outcome`, `disposition`, `runtime_ms`, `reason` and `error`, and a
  present `kind` is exactly `"disposition"`. Any other key - a parity field,
  a diagnostic, one piners adds later - makes it invalid, the key alone, null
  included. For a line that deserializes, the message names the unexpected
  keys; a line whose values do not read into brokkr's typed fields
  (`"matched": null`, a non-string `error`) is invalid with the
  deserialization error instead. A missing or unknown reason, or a missing, blank
  or multi-line error, makes it invalid too. A `harness_abort` line must
  carry a closed `death`: `{"kind":"signal","number":N}` with N from 1 to
  64, `{"kind":"exit","code":N}` with N from 1 to 255, or
  `{"kind":"reply_failure"}`, and no other key inside it. A missing, null or
  malformed death makes the abort invalid, so an abort from a piners that
  predates the field fails as an invalid record rather than as a break. That
  changes nothing for an ordinary run, which the abort failed anyway. It
  does make such a run's diagnostic attempt protocol-invalid rather than
  completed, and a `--bless --commit` at such a commit cannot adopt the
  abort. A `death` on any other line, a null included, makes that line
  invalid. A `compile_fail` or `runtime_fail` line must carry a closed
  string `cause`, one of `script`, `capability`, `budget`,
  `interpreter_fault`, and on `runtime_fail` alone `panic`; a missing,
  null or unknown cause, or `panic` on a `compile_fail`, makes it invalid,
  so a failure from a piners predating the field fails as an invalid record,
  with the same consequences as a deathless abort. A `cause` on any other
  line, a null included, makes that line invalid. Every other line tolerates keys
  brokkr does not read. The record must also agree with itself: when the harness's own
  `disposition` field is **present** it must be exactly the derived label, as
  a string - a different label, a null, a number, an object or an empty
  string makes the record invalid, and the message prints both values. Only
  a **missing** key is tolerated (a harness predating the field). Every
  occurrence is checked before repeats collapse, so a later clean repeat
  cannot hide an earlier contradiction: the probe stays unscored, never
  satisfies its pin (the gate shows `invalid (an occurrence was invalid;
  ...)`), explains no exit-1 break, and is stored with `scored = 0` and the
  `defect` that unscored it, so `corpus-results` lists it as `INVALID`
  whether or not it was selected. A disposition line's `kind`, when present,
  must be the string `"disposition"`: any other present value (a null, a
  number, an object) routes the line here as no other kind, and makes it
  invalid, whatever its outcome.
  The record is still stored whole. The parser validates each line once,
  into a `report::ProbeOutcome` - an evaluated disposition a pin can hold,
  or a refusal no pin, gate comparison or bless stamp can - or the reason it
  is invalid (`report::record_label`, over the outcome/tier primitive
  `report::valid_label`, plus the `unevaluated` fields above). Every consumer
  reads that verdict: the gate (an invalid record never satisfies its pin,
  nor explains exit 1), bless, the summary and the reconciliation;
- **missing** - selected ids with no record at all;
- **invalid** - a line for a selected id whose disposition is not a gate
  label, a disposition line that does not deserialize, a stdout line that
  is not JSON, or a line that repeats a key (below);
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
after it, the divergence and dense-na breakdowns, and the `N probes matching
their pin (hidden)` count all cover the **scored** set only: an extra or an
invalid line is stored, but never counted as a match or aggregated. Its
stored row says so: `scored` is 1 exactly for the scored set and 0 for every
other stored row, with `defect` naming the unusable record - the row's own,
or the occurrence that tainted its probe - and `NULL` for a valid extra
(`docs/projects/piners.md`, the run store). A query over the store keeps to
what the run counted with `scored IS NOT 0` (a row stored before the mark
existed reads `NULL` unless its run counted no protocol violation).

**Repeated keys.** A line is one JSON object written once: a key repeated
within any object of it, at any depth, makes it a record the harness did not
write once, and nothing brokkr reads from it is trusted. A repeated
top-level `kind` (nothing to route the line by) and a disposition line with
any repeat are invalid records, named by their `probe` unless that key
repeated, and never stored as a row (the repeat is kept in the run's
`report_warnings`, kind `duplicate_key`, with the repeated paths); a contract
line is still a contract line, but a violation whose fields are not read
(below); a repeated top-level `summary` denies the legacy-summary skip; a
`trade_diff` line is warned about and dropped, like one whose key does not
parse; an unknown kind is skipped unread.

Invalid records, duplicates and extras are **protocol violations** and fail
the run. **Any selected probe without a valid disposition fails the run**,
even on exit 0 and even under `--no-gate`: a probe that vanished is not a
probe that passed. (A `trade_diff` line whose key - `probe`, `our_index`,
`tv_index` - fails to parse is still only warned about and dropped: it is a
diagnostic, never scored.)

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
stored in the run's `protocol_violations` count.

If the run's own `harness.stdout`/`harness.stderr` cannot be written into its
dir, both writes are still attempted and every failed path is named. The
captured output is still parsed and ingested (`wall_ms` stays the original
attempt's), but the run fails with `evidence storage failed: ...` in its
`fail_reason`, bless is refused, the failure counts in
`protocol_violations`, isolation does not
run (an explicit evidence-storage stop, recorded in the run's `diagnosis`),
and the dir is kept and reported as `artefacts preserved with incomplete
evidence (failed: <files>)`.

## The harness contract

The harness's versioned contract lines (shape in `docs/projects/piners.md`)
are checked in the same one run assessment as the reconciliation
(`src/piners/integrity.rs` builds it, `src/piners/contract.rs` validates the
contract). Every consumer reads that one assessment under its own acceptance
policy: an ordinary run and bless (the failure reasons above), and an
isolation attempt.

- **Recognition before version.** A line whose `kind` is one of
  `run_start`, `setup_stage`, `setup_complete`, `probe_start`, `probe_end`,
  `run_end`, `run_error` is a contract line whatever else it says (a `summary: true`
  flag included). The legacy-summary skip applies only after this, and only
  to an actual legacy summary record: `summary: true`, no `probe` key, and no
  `kind` or a string kind other than `disposition`. A disposition record
  carrying a `summary` flag is still a disposition record; a missing,
  malformed or unsupported `version` (anything but integer 1) is a
  violation, even if no valid contract line appeared, and so is a contract
  line that repeats a key: it still counts as present (a `run_start` is
  still a start, and a second beside it a duplicate), but none of its fields
  is read, so a repeated-key `run_error` is never the run's record and a
  repeated-key `probe_start` starts nothing. Unknown kinds are still skipped;
  a non-string `kind` is no kind, so such a line is neither a contract line
  nor skipped.
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
  the hang backstop or an interrupt - a backstop kill is classified as such,
  never as a spontaneous SIGKILL); it must be the last
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
  reports enters the dispositions, the gate or bless.
  Afterwards the run row's `diagnosis` gets a note - the isolated probes, the
  diagnosis status, and where the artefacts are - beside the untouched
  `fail_reason`. An interrupt during diagnosis cannot lose the original row.
- **What is bisected.** Only the original run's *unreported* selected ids
  (said in the output). Same built binary, same lock, same env, flags, cwd and
  forwarded harness args. Each attempt gets its own `attempt-<n>/` under the
  run dir, with its own manifest (built from the same verified probes),
  `harness.stdout` and `harness.stderr`. Each
  attempt gets the same run assessment as the original. A subset *completes*
  only if it exits 0 or 1, validly reports every probe in it (a
  `harness_abort` counts: the harness already attributed that crash), breaks
  no report protocol or contract, carries its `run_end` once the contract is
  observed, and its stdout was not cut short; a failing subset is split in
  two and each half run alone. An `unevaluated` line is a valid report, so
  its subset can complete, but every refusal an attempt's captured stream
  holds is kept beside the attempt's outcome, whether the attempt completed,
  failed or stopped diagnosis (an evidence-storage failure or the deadline
  included; an attempt that never produced a capture has none). Each is kept
  with the attempt that reported it, never merged across attempts, so one
  probe refused for two reasons keeps both: the console lists `unevaluated
  (...): id (reason) in attempt N: detail` and the stored note `unevaluated:
  id (reason) in attempt N`. A singleton whose own attempt refused it reads
  `probe X reported unevaluated (reason) when run alone in attempt N`, never
  `completed when run alone`. A failure is classified, in this order:
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
  requested shutdown, the deadline, a failure to spawn or prepare an attempt,
  or an attempt that exceeds its stdout/stderr capture bound; a diagnostic
  timeout is never attributed to a probe. On the cap or the deadline it prints
  the unresolved subsets and why it stopped.
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
the only way the file is created or its hashes refreshed. No build/harness;
`--no-gate`, `--debug`, `--release` and `--profile` are therefore refused with reseed.
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

Prints `added/changed/removed` and the number of feed file hashes that changed.
Touches the pinned *content* only: re-hashes the probe files, the selected
probes' `[feeds]` groups and every other group naming one of their files
(so a replaced file shared between groups cannot leave one stale; `--all`
refreshes every group), and every
`[harness_files]` entry (`--probe` included - an edited facts file is
re-stamped by any reseed), preserves
`[probe_config]` verbatim, and carries each surviving probe's `expected`
and ratchet counts (`labels`, `performance`, `excursions`) forward, with a
failure's `cause` and an abort's `death` and `attribution`. It decides nothing about how a probe runs, so a newly discovered or
re-added probe runs under whatever `[probe_config]` declares for its
directory and stays unblessed. A blessed probe that gained or lost a
`record` - so is now judged against a different oracle - keeps its
`expected` and ratchet counts but is named in a re-bless warning, since a disposition or count that
happens to match across the switch would otherwise pass the gate unnoticed.
A content change that breaks a `[probe_config]` rule - the probe gained a
`record` under a declared CSV timezone or bar budget, or the last probe under an exact-dir
declaration vanished - refuses the reseed, naming the declaration to edit.

`--bless [--all|--keyword <k>|--probe <id>]` runs the selection (verify +
build + harness), then stamps each probe's current disposition into
`expected`, and each census's per-leg `mismatched` counts into `labels`,
`performance` and `excursions` (the ratchets, above): a non-parity line
clears them, and a parity line without a leg leaves an unpinned leg unpinned
(a harness predating a census still blesses) but refuses the bless where the
pin already holds a count it would drop. A `compile_fail` or `runtime_fail`
is stamped with the `cause` its line reports, and any other stamp clears
`cause`. A `harness_abort` is stamped with
the `death` its line reports and an attribution:
`--attribute PROBE=TEXT` (repeatable, once per probe, split at the first
`=`, one nonblank line) supplies it, and is required where the abort is new
to the pin or its death moved, since the old statement was written for
another death. On an unchanged abort the pinned attribution is kept, or
replaced by a flag. A flag naming an unselected probe, or one whose line is
not an abort, refuses the bless. Any other stamp clears `death` and
`attribution`. `changed` counts a probe whose
`expected`, cause, death, attribution or any ratchet count moved.
Records reality including fails (a probe exercising an
unimplemented feature legitimately pins `expected = "compile_fail"`; the
gate then catches it starting to compile). Never gates. Prints `blessed N
(changed M)`. Excludes `--verify-only`/`--reseed`. Bless is all or nothing:
it refuses the **entire** write - before a single pin is touched, in memory
or on disk - unless the harness exit is acceptable, the report has no
protocol violation, and every selected id has a valid, stampable
disposition. Exit 1 is acceptable only when the report carries the break
lines it signals (recording them is the point); exit 2, any other code, a
signal, the hang backstop, a `run_error`, a missing or invalid record, a
repeated record, a report-only extra, a contract violation or any
`unevaluated` probe leaves `pins.toml` untouched and exits non-zero, naming
why, and so does an abort missing the attribution it needs. A
`harness_abort` is otherwise stampable like any break; an `unevaluated` line is
not, since an experiment that was not conducted records nothing about the
engine. The run row records `gated = no` - bless ignores the gate verdict.

Bootstrap: `--reseed --all` -> hand-write `[feeds]` groups, any
`[harness_files]` paths and the `[probe_config]` declarations -> `--reseed
--all` again (stamps feed and harness-file hashes)
-> commit -> write keyword files -> `--bless --all` -> commit -> runs are gated.

## Exit codes

Harness exit: `0` every probe scored, `1` every probe scored and at least one
break (`compile_fail`, `runtime_fail`, `harness_abort`, `unevaluated`), `2` a
`run_error`.
brokkr exits non-zero on a `run_error` or any `unevaluated` probe (always,
`--no-gate` and bless included), on a harness exit the pins do not explain - `1` with any break
not pinned to itself (see pinned breaks, above), `2`, any other code, a
signal (an abort after complete reporting included), or the hang backstop -
on a contract violation, on a report-integrity failure (a selected probe
without a valid disposition, an invalid record, a repeated record, a
report-only extra, or
output cut short), **or** on an active gate deviation. The integrity checks
apply on exit 0 and under `--no-gate` alike. Hash mismatch fails earlier
(before build). `--no-gate` and `--bless` never fail on gate diffs; `--bless` fails
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
(probes scheduled slowest first by estimated scan duration, so the harness -
which fills its slots from the front of the manifest - is less likely to
start a slow one last; see "Probe scheduling") plus captured `harness.stdout` / `harness.stderr`; every run's NDJSON is
ingested into `runs.db`, so the dir is dropped once ingest commits - unless
`--keep-artefacts`, or diagnostic isolation ran (its `attempt-<n>/` dirs are
not in `runs.db`, so the dir is preserved and its location printed). A run
that ends before its output can be ingested
(interrupted, the harness failed to spawn, the manifest could not be
written) is still recorded under its id (`result` `interrupted`, `fail` or
`error`) and keeps its dir. Only a run killed outright (SIGKILL, a crash) or
whose ingest itself failed leaves a dir with no row - which keeps its number
taken until `brokkr clean` removes it. Clean removes the `run-*/` dirs but
spares `runs.db`. Each ingest keeps a fixed number of the newest run IDs in
that database (`RUN_RETENTION_LIMIT` in `src/piners/corpus_db/ingest.rs`) and
prunes older runs together with their disposition and diagnostic rows.

## See also

- `docs/brokkr.toml.piners.md` - the `[piners]` config block.
- `docs/projects/piners.md` - harness NDJSON + manifest contracts,
  `runs.db`, the `brokkr corpus-results` query surface.
