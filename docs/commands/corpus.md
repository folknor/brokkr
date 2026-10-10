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
emitted a disposition line for every probe it was given, it ran with no
forwarded harness flags, and it was built in the same profile as this run (a
row predating the recorded profile counts as debug). Otherwise one fast-failing
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
indefinitely.

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
`--allow-drift`; re-stamp with `--reseed` or fix the tree.

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

**Pinned breaks.** The harness exits 1 whenever any probe breaks, including a
probe pinned `expected = "compile_fail"` that is doing exactly what its pin
says. brokkr therefore accepts exit 1 when every break line in the report
belongs to a selected probe pinned to that same break; an unpinned, mispinned
or unselected break still fails the run (gated or `--no-gate` alike).

**Repeated records.** The contract is one disposition line per probe and one
`trade_diff` line per `(probe, our_index, tv_index)`. A repeat is kept as its
last occurrence for display and storage, named in the output, and fails the
run (`N repeated harness records`, `1 repeated harness record` for one).

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
(changed M)`. Excludes `--verify-only`/`--reseed`. A bless run whose harness
failed stamps nothing and exits non-zero: exit 1 is acceptable only when the
report carries the break lines it signals (recording them is the point), and
exit 2, any other code, a signal, the hang backstop, or a repeated record
leaves `pins.toml` untouched. The run row records `gated = no` - bless
ignores the gate verdict.

Bootstrap: `--reseed --all` -> hand-write `[feeds]` groups, any
`[harness_files]` paths and the `[probe_config]` declarations -> `--reseed
--all` again (stamps feed and harness-file hashes)
-> commit -> write keyword files -> `--bless --all` -> commit -> runs are gated.

## Exit codes

Harness exit: `0` clean, `1` compile/runtime break(s), `2` harness error.
brokkr exits non-zero on a harness exit the pins do not explain - `1` with
any break not pinned to itself (see pinned breaks, above), `2`, any other
code, a signal, or the hang backstop - on a repeated harness record, **or** on
an active gate deviation. Hash mismatch fails earlier (before build); the
runtime-ceiling refusal after verification but before the build. `--no-gate`
and `--bless` never fail on gate diffs; `--bless` fails (and stamps nothing)
on a failed harness. `--verify-only` exits 0 once all pins (and feeds and
harness files) verify.

## Artefacts

Each run is numbered once: its dir is `.brokkr/piners/corpus/run-<id>/` and
its row in the corpus run store (`runs.db`) has the same `run_id`, so the
number a run prints (`run <id> -> <dir>`) is the one `corpus-results`
takes. The id is reserved under the lock, one past both the highest stored
run and the highest `run-<id>/` dir on disk. The dir holds `manifest.json`
plus captured `harness.stdout` / `harness.stderr`; every run's NDJSON is
ingested into `runs.db`, so the dir is dropped once ingest commits - unless
`--keep-artefacts`. A run that ends before its output can be ingested
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
