# Tiered check: a fast answer that admits it, and a slow answer worth trusting

Status: in progress. Landed so far: feature 5; feature 8's first slice (the
`--json` summary trailer, `schema: 1`); and build-order step 3 - `certifies`
with the full permission table and interim complete rule, feature 1
(`skip_phases`), and feature 9 (`gate_profile` / `--gate`), including the
0/10/1 exit contract and the `passed`/`complete`/`partial` verdict split.
Everything else is proposed.

Scope: `brokkr check` and `brokkr test`. Every mechanism here is opt-in. No
existing key changes meaning, and a `brokkr.toml` that does not ask for any of
this behaves exactly as it does today.

## Problem statement

`brokkr check` is the single local gate between a code change and a PR sent to
an external maintainer. It is what `AGENTS.md` points every agent at, and what
a human runs before handing over a commit. When it prints `check passed`, that
sentence is load-bearing: it is the basis for claiming a change is ready to go
upstream.

Five things are wrong with it at nautilus_trader's scale.

### 1. It is too slow to run in the loop, so it isn't

A warm run is 4m02s. A run that rebuilds is 6m30s to 8m10s. Nobody waits four
to eight minutes between two edits to the same function, so in practice the
gate runs once, at the end. Errors a 20-second check would have caught during
the edit get caught after the work is nominally finished, or not caught locally
at all and found by CI.

The recorded baseline is at the top of `TODO.md`.

### 2. It is about to get significantly slower

The madsim sweep is unblocked (feature 5 landed). By construction it cannot
share a target directory with the other three sweeps, because `--cfg madsim`
changes the whole-graph fingerprint. Adding it to the standing gate means a
near-cold build of five crates and their graph, plus seven curated run legs, on
top of the current cost.

### 3. The gate is incomplete, and the incompleteness is invisible

- `tier1` skips 14 tests.
- The `serial` profile exists but no profile invokes it, so the standing gate
  has never run the `serial_tests::` family at all.
- `doctests = false` disables another 55 tests in persistence alone.

None of this is visible in the output. The run prints `check passed`, and the
reasons live in prose comments in `brokkr.toml` that only a reader of that file
will ever see. "Green" currently means "green over a set that has been
shrinking, by amounts recorded nowhere machine-checkable".

### 4. Some of what is skipped is hiding defects, not flakes

The five SBE proptest roundtrips fail because the strategies draw from the
128-bit model domain while the wire format is 64-bit - a genuine contract bug.
`test_twap_calculates_size_schedule_with_remainder` is parked on B14, a
corroborated-High bundle. Skipping these is suppressing signal, not deferring
noise. Nothing in the output distinguishes the two cases.

### 5. The slowness has a specific cause, and it is not "28,929 tests"

Durations cluster on exact constants - 15.0s, 10.0s seven times, 7.1-7.3s
eleven times, 5.047s ten times - which is the signature of fixed sleeps and
timeouts, not of work. The 5.047s family is betfair's mock venue handler doing
a literal `sleep(Duration::from_secs(5))` once per test.

Roughly 285s of the run is a few dozen tests waiting on a clock.

"Make the gate faster" and "fix the tests" are separable projects with
different owners. The second is upstream's as much as ours, since it costs
their CI too.

## The tension

Speed and trustworthiness pull in opposite directions, and the obvious lever
for the first - run less - attacks the second directly.

Two different guarantees are needed from one tool:

- a **fast** answer that is allowed to be partial and must say so, and
- a **slow** answer that is complete and can be relied on.

Today there is one setting and it is neither fast enough to use nor complete
enough to fully trust.

## Design principle: the claim is primary, not the selection

The natural instinct is to add knobs that answer "what runs": which phases,
which sweeps, which packages, which lanes, which conditions. That is five
mechanisms answering one question, two of them consuming the changed-file set
with opposite polarity. After they all land, "why did this run these four
things?" requires consulting all five.

Worse, they all attack **selection** while problem 3 is about **claim**: what a
green result means.

So the organising concept is a declaration of what a profile certifies, with
permissions derived from it:

```toml
[test.profiles.full]
certifies = "complete"

[test.profiles.edit]
certifies = "partial"
```

| | `complete` | `partial` |
|---|---|---|
| package scoping (`scope`) | rejected | permitted |
| `skip_phases` | rejected | permitted |
| unaccounted skips | rejected | permitted |
| conditional sweeps (`when`) | permitted, never in the certified floor (feature 6) | permitted |
| may print `passed` | yes | no |
| exit code on success | 0 | 10 |

Two consequences matter more than the table.

**Violations fail at resolve time, not in a trailer.** An unquarantined skip in
a `complete` profile is a *config error*, reported before a single crate
compiles. A gate that tells you after four minutes that it was not really a
gate is a report.

**There is no third rule to remember.** `partial` gets both permissions,
`complete` gets neither. This is why `certifies` replaces the earlier sketch's
two bolt-ons (`enforce = true` on coverage, plus `scope = "full"` marked on
gate profiles): one invariant enforced from two places is how you end up with a
profile that is `enforce = true` and silently scoped.

**The table governs flags, not just keys.** `brokkr check` already carries CLI
scoping today - `-p/--package` (`cli/schema.rs:84`), `--features`,
`--no-default-features` - and a permission model that only reads `brokkr.toml`
reopens the B41 hole from the command line: `brokkr check --gate -p foo`
printing `passed` would be the central lie, typed by hand. Under a resolved
`complete` profile these flags are rejected exactly like `scope`; under
`partial` they are the manual scoping story (feature 3).

Profiles without `certifies` behave exactly as today: no new permissions, no
new restrictions, no accounting.

**Where the new keys live: `[test.profiles.*]`, deliberately.** The layering
objection - `skip_phases` and `scope` govern textlint and clippy, which are
not test-phase concerns, so shouldn't a whole-check profile get its own
top-level table? - is acknowledged and overruled. Profiles already steer more
than the test phase: sweep selection honours the profile today
(`decide_active_sweeps` in `check_cmd/output.rs`), so the clippy phase is
already profile-shaped. `certifies` widens an existing wart; a new top-level
`[profiles.*]` would instead be a second table answering a question
`[test.profiles.*]` already answers - the same two-mechanisms smell this
design rejects for selection knobs and for a magic default-profile name - and
would need precedence rules plus a migration for every existing config. If
the parent name ever becomes intolerable, the fix is a rename-with-alias
migration done as its own change, not smuggled into this one.

### Why scoping must be gated: B41 is this hazard, already fired

Package-scoping does not merely test less. It changes the **build**, because
cargo unifies features across whatever set is selected. A scoped run's green is
therefore not a subset of the full run's green; the two are *incomparable*, in
both directions.

This is not a theoretical objection. It is a backlog entry. `cargo test
--workspace` pulls `nautilus-blockchain`, which hard-enables
`nautilus-model/defi`, which widens `Price`/`Quantity` raws to 128-bit, so a
workspace run decodes 64-bit fixtures with a 128-bit decoder and six
`test_catalog` tests fail - while the same tests pass under `-p
nautilus-persistence`. That is scoped-green and full-red on identical source,
in the direction people assume is impossible.

Cited here so it is not relitigated later. Scoping is acceptable for a loop
answer and unacceptable for a gate, and `certifies` is what draws that line.

## Proposed features

### 1. `skip_phases` (subtractive, `partial` only)

```toml
[test.profiles.edit]
certifies = "partial"
skip_phases = ["script_check", "textlint"]
```

Subtractive, not an allowlist. An allowlist (`phases = [...]`) means adding a
new native phase to brokkr silently excludes it from every profile written
before that phase existed - a quiet-coverage-loss mechanism inside the feature
meant to prevent quiet coverage loss. New phases must be opt-out.

Rejected under `certifies = "complete"`: a complete profile that skips phases
is claiming something it did not check.

Note the marginal value is smaller than it looks. `brokkr test` and `brokkr
clippy` already exist as standalone commands, so the two expensive phases are
already independently runnable. On nautilus this uniquely buys skipping
`script_check` (which shells out to a bash script) and textlint.

### 2. `lanes`: profile composition

```toml
[test.profiles.pre-commit]
certifies = "complete"
lanes = ["tier1", "serial"]
```

Closes the serial-lane hole (problem 3) without a new verb and without touching
`tier1`. `--profile` already exists, so no CLI change.

Named `lanes`, not `includes`: `extends` already exists with merge semantics,
and this is the opposite operation. `tier1` is parallel-with-skips and `serial`
is single-threaded-with-`only`; the two filter sets are contradictory by
construction, so no merge can express them. A profile with `lanes` is a *list
of runs*, not a merged run.

Implementation cost, which is the real work here: **clippy is per-sweep, tests
are per-lane.** Two lanes sharing the `default` sweep must not clippy it twice.
The clippy phase dedupes on `(packages, features, rustflags, env)` while the
test phase keeps both entries. `env` is part of the key: `HIGH_PRECISION=1` on
one sweep and not another makes two otherwise-identical sweeps
cache-incompatible.

Composition rules, fixed now so implementation does not fix them by accident:
a profile with `lanes` carries no run-shaping fields of its own (`sweeps`,
`tests`, `only`, `skip`, `include_ignored`, `test_threads`, `env`, `extends`
are all rejected beside `lanes`); a lane-referenced profile declaring
`certifies` is an error (certification belongs to the composing profile);
lanes do not nest. And the dedupe key is the **whole build shape**, not the
four fields above: `no_default_features` and `build_packages` (both used by
pbfhogg today) change the build product and belong in the key.

### 3. `scope = "changed"` (`partial` only)

```toml
[test.profiles.edit]
certifies = "partial"
scope = "changed"
```

The dirty set is the **input, never the answer**. #4530 is the case to design
against: every edit was in `crates/model`, but it added a trait method consumed
across the workspace. Testing the dirty crate would have been green regardless
of what it broke in `execution` or `live`.

Resolution: changed files -> owning packages -> transitive reverse-dependency
closure -> sweep and test selection. Both halves already exist in brokkr:
`scope::changed_files()` computes the changed set today for `--limit` scoping
(merge-base diff plus uncommitted working-tree changes), and the dependency-rule
phase already shells `cargo metadata`.

Four rules built in from the start:

- **Non-Rust changes escalate to full scope.** A `brokkr.toml`,
  `.pre-commit-hooks/`, or workspace-manifest edit invalidates the premise;
  fall back rather than guess.
- **An empty changed set escalates to full scope.** A clean tree immediately
  after a commit is the *common* state for this profile, and `check partial
  (0/45 packages)` on it reads as breakage. The honest reading of "check what
  changed" when nothing changed is "there is nothing to narrow to", not "check
  nothing". Escalation widens the build, never the claim: the run still
  certifies `partial` and exits 10 - scoping was not the profile's only
  permission in play - and the trailer records that scope escalated and why.
- **A scoped pass is not a pass.** Different word, different exit code, and it
  appears in the machine-readable output (feature 8). The failure mode that
  matters is not a slow gate, it is handing over a green that only covered
  `crates/model`.
- **Scoping is rejected under `certifies = "complete"`,** for the B41 reason
  above.

This scopes the **build**, not merely the test selection. Keeping `--workspace`
and restricting only libtest filters would preserve feature parity but save
nothing when cold, which is the case that hurts. Scoping the build is the point,
and `certifies` is what makes it safe to say out loud.

**Two demotions, recorded after review.** First, the prior question this plan
never asked: without `scope`, is `edit` actually fast? No - `sweeps =
["default"]` is still the whole workspace (28,929 tests), so `skip_phases`
alone turns 4m02s into two-something minutes, which is not a loop. Second, the
interim answer already exists: `brokkr check -p` is manual scoping, shipped
today, with no inference machinery and a package set that stays stable while
work stays in one crate - stable in exactly the way an inferred set is not.
That stability matters mechanically: consecutive runs with *different* `-p`
sets re-unify features per set and can recompile shared dependencies under
different feature shapes in the same target dir - B41's mechanism pointed at
the cache instead of at correctness. The feature meant to make the loop fast
can thrash the cache that makes the loop fast, precisely when the changed set
is churning, which is what a loop is.

Feature 3 is therefore automating something already available, not enabling
something new. It is **conditional, not merely last**: measure the loop as
`--profile edit` plus manual `-p` first, and build the inference only if the
measured gap justifies both the machinery and the thrash risk.

**Measured** (nautilus, 2026-07-22, first real partial run): the `edit`
profile - one sweep instead of three, textlint skipped - saved 50.5s of
268.4s, 18.8%. 3m37s is not a loop: phases and sweeps alone do not get
there, because the default sweep is still the whole workspace. Scoping is
the lever. The same run also caught manual `-p` broken under profiles:
cargo *unions* selection flags, so a sweep emitting `--workspace --exclude
…` silently swallowed the CLI `--package` - scope recorded in the trailer,
not applied, on the first day the trailer existed to record it. Fixed: a
CLI `-p` now replaces the sweep's selection, out-of-scope sweeps skip with
a log line (mirroring `brokkr test`'s SKIP), an all-sweeps skip fails
instead of reading green, and the shape line plus the `--json` `package`
field both carry the scope. Manual `-p` is thereby a working interim
answer, and the feature-3 gate measurement (`edit` plus `-p`) is runnable.

### 4. Coverage accounting: make `skip` unable to hide

`--timings` proves brokkr already enumerates every test. Orphan detection needs
one further primitive: a libtest `--list` pass per sweep with filters off, then
subtract each lane's filtered set. `test_cmd.rs` already runs exactly this
enumeration for its ambiguity check, so the machinery exists; it is a
no-execution pass over already-built binaries.

**The unit of coverage is the (sweep, test) pair, not the test name.**
Name-level subtraction certifies too much: `serial_tests::` matches `serial`'s
`only` textually, but `tier1` skips it across three sweeps (`default`, `ffi`,
`live`) while `serial` runs `sweeps = ["default"]` - so the `ffi` and `live`
builds of every `serial_tests::` test are skipped everywhere, run nowhere, and
a name-keyed accounting would call them covered. That is the B41 argument
turned on the accounting itself: a pass under one feature graph is not evidence
about another, which is exactly why scoping is gated under `partial`. The
`--list` pass is already per-sweep, so the data exists; it is the subtraction
that must keep the pair. "Runs elsewhere" accordingly means the *same pair*
runs in another lane, not the same name.

**The universe is every `[[check]]` entry, not the profile's own sweep list.**
The pair rule fixes one granularity; the sweep list is the other. If the
universe were defined by the sweeps the profile's lanes happen to reference,
deleting a sweep from a lane would shrink the universe and the audit with it -
green over a set that has been shrinking, at sweep granularity, inside the
auditing feature. So: universe = all `[[check]]` entries x their enumerated
tests. An unconditional entry referenced by no lane of a `complete` profile is
a resolve-time error; a `when`-carrying entry is accounted through feature 6's
omission reporting.

**`#[ignore]` is a third suppression channel, and it is already live.** 19
files under nautilus's `crates/` carry `#[ignore]`, including three in
`live/tests/stress.rs` and one in `persistence/tests/test_catalog.rs` beside
the B41 family - and the two commands already disagree about what exists:
`brokkr test` always passes `--include-ignored` while tier1's sweeps do not,
so one command can run a test the other command's accounting would never
enumerate. Under `complete`, either every lane sets `include_ignored = true`
or ignored tests are enumerated and reported as accounted omissions. The
subtle part is the enumeration pass itself: libtest's `--list` honours the
same filters, so the pass must pass `--include-ignored` or it inherits the
blindness it exists to remove.

```toml
[test.coverage]
enforce = true   # implied by certifies = "complete"

[[quarantine]]
pattern = "test_twap_calculates_size_schedule_with_remainder"
issue   = "B14"
reason  = "expectation width-sensitive under unified 128-bit build"
```

Under enforcement every `skip` entry must be justified by either another lane's
`only` (it runs elsewhere) or an explicit quarantine entry. The run trailer
reads `12 quarantined, 0 orphaned`.

`issue` is **required**, not decorative. It is what turns the list from a
graveyard with good manners into a countdown that cannot silently grow, and the
IDs already exist (B14, B41, B42, B43, B49). It is also the mechanism that keeps
a fixed bug fixed: when the SBE width bug is closed, its four entries must be
deleted or the count stops matching.

Staleness is detected mechanically, in both directions. An entry matching zero
(sweep, test) pairs is a load-time error, in the style of the existing
`default_profile` typo check - that is what actually forces deletion when a
bug closes, rather than the count "stopping matching" by hope. And because
patterns are substrings, an entry can silently *grow*: `test_bar_roundtrip`
also matches a future `test_bar_roundtrip_v2`, so a new suppressed test can
ride an old justification. The trailer therefore reports per-entry match
counts, per pair - an entry matching under `default` but not under `ffi` is
half-empty, and a count that rises without an edit to the list is the growth
signal.

Note the one-time migration: `skip` entries are substrings, so "runs elsewhere"
is substring matching over pairs, not set arithmetic. The roughly 12 named
skips (`test_quote_tick`, `test_bar_roundtrip`, the TWAP one) match nothing
anywhere and will each need an entry on day one - and the `serial_tests::`
`ffi`/`live` pairs surface as orphans immediately, to be closed by either
widening `serial`'s sweeps or quarantining them with an `issue`. Both are the
feature working as designed: the second is a real coverage hole the name-level
sketch would have certified over.

**Doctests live inside the accounting, not beside it.** `doctests = false` is
today a bare boolean disabling 55 tests in one crate, invisible to a `--list`
pass. Under a `complete` profile it must itself carry a quarantine entry with an
`issue` (B42) and a reason, exactly like a skip. Otherwise the run prints `0
orphaned` while 55 tests sit outside the universe - reintroducing problem 3
inside the fix for problem 3.

### 5. Per-sweep `rustflags` with automatic target-dir isolation

**Already landed** (commit 16b2c80). Recorded here for completeness.

```toml
[[check]]
name = "sim"
packages = ["nautilus-common", "nautilus-core", "nautilus-network",
            "nautilus-execution", "nautilus-live"]
features = ["simulation"]
rustflags = ["--cfg", "madsim"]
```

A sweep with non-empty `rustflags` auto-isolates into its own target dir. The
key is a hash of the **flag content** (`target/rustflags-<hash>`), not the sweep
name, so sweeps carrying identical flags share one cache instead of paying two
full compiles. Setting `RUSTFLAGS` or `CARGO_TARGET_DIR` in `env` alongside is
rejected at parse time.

`[[check]]` also already carries `tests`, `skip`, and `only`, so per-check
test-name selection exists. One residual gap: those filters are **per-sweep, not
per-package-within-sweep**, so the sim plan's seven legs with different filters
per package set need seven `[[check]]` entries, not one.

### 6. Conditional sweeps

```toml
[[check]]
name = "sim"
when = { packages_changed = ["nautilus-common", "nautilus-core",
                             "nautilus-network", "nautilus-execution",
                             "nautilus-live"] }

[[check]]
name = "cold-full"
when = "manual"
```

Runs when the dirty set intersects the named packages, or on demand via `--with
sim`. This is where the dirty-set idea genuinely earns its place: as an
**additive trigger for an expensive sweep**, not a subtractive filter on a gate.
It discharges problem 2 on its own.

No `optional` key: a sweep carrying `when` is conditional by definition, and
`optional = false, when = {...}` has no meaningful reading. `when = "manual"` is
the explicit variant for a sweep that should never fire automatically - the
first thing anyone does with a cold madsim sweep is run it deliberately, not
wait for a trigger.

`when.packages_changed` consumes the same changed-files -> packages mapping as
feature 3. Build that mapping here first: this feature is additive, so its worst
case is an expensive sweep running when it need not, whereas feature 3's worst
case is a false green.

**Pricing `when` under `certifies`.** Left unpriced, a conditional sweep inside
a `complete` profile makes the certified set vary with the dirty set: two
`complete` greens on the same tree, reached from different branches, would not
be comparable. Rejecting `when` under `complete` is no answer - the standing
gate is exactly where the madsim sweep wants to live, so that reinstates
problem 2. The resolution is that conditional sweeps are **never part of the
certified floor**: `complete` certifies its unconditional sweeps, always. A
fired conditional sweep runs and can fail the run, but it is a recorded extra,
not part of the claim; a non-fired one is an accounted omission that appears in
the trailer and in the feature-8 summary (per-sweep `fired` status) the way a
quarantine count does. Additivity protects against wasted work; this accounting
is what protects against a shifting definition of "complete".

The changed set is feature 3's definition: merge-base diff plus uncommitted
working-tree changes. One caveat, named because it lands exactly where the
non-goal works hardest: on a branch workflow (nautilus, `fix-*` off
`develop`) the merge-base diff stays non-empty after committing, so a
post-commit gate still fires its conditionals. On a commit-to-master workflow
(brokkr's own projects) the post-commit changed set is empty and conditional
sweeps never fire after the commit - the omission accounting above is what
keeps that visible rather than silent.

### 7. Slow-test budget, reusing the quarantine shape

```toml
[test]
timings_threshold = "3s"

[[slow]]
pattern = "betfair::"
issue   = "B49"
reason  = "mock venue handler sleeps 5s per test"
```

A bare threshold that only prints is the same failure mode as the prose
comments in problem 3: true, visible, ignored. But a bare threshold that
*fails* is a flake source - 3s on an idle machine is 6s on a loaded one, and a
gate that fails on machine load is a gate people learn to rerun until green.

So it takes the same shape as feature 4: known-slow tests carry an entry with an
`issue`, and the gate fails only on **unlisted** tests crossing the threshold.
B49's betfair sleeps go in the list, the list shrinks as they are fixed, and a
newly-slow test fails loudly. One concept, two applications.

The residual flake is the borderline case: an unlisted test that has always
run 2.8s crosses 3.0s on a loaded machine and fails - the rerun-until-green
habit reintroduced inside the feature built to prevent it. The fix is to make
the detector relative, not absolute: a **multiplier over a recorded per-test
baseline**. A test that was 0.4s and is now 3s is the signal; a 2.8s test that
was always 2.8s is not - and fixed sleeps, the actual population of problem 5,
are load-insensitive, so a generous multiplier separates them from compute
noise cleanly. Priced honestly: this needs a per-test, host-keyed baseline
store with a bless step (the ratatoskr gate baselines are the in-repo
precedent for exactly this shape), and a test with no recorded baseline falls
back to the absolute threshold. That store is real machinery - a second
reason this feature sits last in the build order.

Always list tests over the threshold in the trailer regardless of profile.

### 8. Result contract

`AGENTS.md` points agents at this gate, and this session parsed stdout for
`check passed` to determine an outcome. That is not a thing that should be
load-bearing.

- Re-add `--json` to `check` (it was removed; re-adding is cheap), emitting one
  machine-readable summary object: `certifies`, packages covered vs total,
  lanes run, quarantined count, orphaned count, conditional-sweep `fired`
  status, slow-test violations, and the pass/partial/fail verdict.
- The object is **versioned and additive from the first commit**: it carries
  `schema: 1`, and consumers must tolerate unknown fields. This feature lands
  second in the build order, before most of the fields it reports exist -
  `certifies` arrives in step 3, quarantined/orphaned counts in step 5,
  conditional-sweep status in step 6. Those land as *added* fields under the
  same schema number; a schema bump is reserved for renames or semantic
  changes, which the additive rule exists to make unnecessary.
- Exit codes: **0 = complete pass, 10 = partial pass, 1 = failure.**

The exit split is not cosmetic. If `certifies = "partial"` returned 0, then
`brokkr check --profile edit && git commit` treats the loop answer as a gate
answer, and the guarantee evaporates in the most common shell idiom there is.
Naive chaining must fail closed; anyone who wants to chain off a partial run has
to say so explicitly.

The success word follows the same split: `check complete` versus `check partial
(4/45 packages, 3 lanes skipped)`. `passed` is reserved for `complete`, because
a consumer skimming output or grepping for a word must not be able to land on
the wrong one.

**Profiles without `certifies` are outside the tri-state contract.** They keep
today's binary exit (0 on success, 1 on failure) and today's `check passed`
wording; `--json` reports `certifies: null` for them. This is the non-goal
made concrete at the one feature that touches every run: pbfhogg's
`default_profile = "tier1"` skips four test families and must not start
exiting 10 the day brokkr updates. The reserved-word rule is therefore a rule
*among certified profiles* - declaring `certifies` is what opts a profile into
the stricter vocabulary, and a legacy `passed` is exactly as trustworthy as it
was yesterday, no more.

**The exit-10 bet, named.** The partial profile is the default,
high-frequency path, and essentially every harness treats nonzero as failure -
the named consumer included: reviewing this design, the nautilus agent hit two
nonzero exits and investigated both as breakage, which on a successful partial
run is wasted work every iteration. The split holds anyway, because exit 0
makes `&& git commit` silently unsafe and that failure reaches a maintainer,
while this one only costs the loop. But it is the plan's biggest UX bet, and
feature 8 landing second is the chance to validate it: run a real agent loop
against the tri-state contract before anything is built on top, and let the
consumer report whether it degrades the loop. First real-use observation
(nautilus, 2026-07-22): the harness surfaced both successful partial runs
as error blocks, exactly as predicted. The bet stands - exit 0 would make
`&& git commit` silently unsafe, and that failure reaches a maintainer -
but the loop cost is now measured fact, not forecast. Precondition, equally
unglamorous: survey the `DevError::ExitCode` namespace before claiming 10.
(Done: `check` itself uses only 0/1; 2 is clap's usage-error code and
elivagar `regress`'s `REFUSED`; 130 is interrupt/`kill`; the dispatch layers
pass through child exit codes, but those are other commands. 10 is free.)

### 9. Invocation: bare `check` is the loop, `--gate` is the gate

The high-frequency action must be the zero-effort one - a fast lane you opt
into by typing a flag is problem 1 with extra steps. Making bare `brokkr
check` the *partial* answer is only safe because of feature 8: a partial
result cannot print `passed` and exits 10, so `brokkr check && git commit`
fails closed with no flag, no discipline, and no memory required. This feature
therefore rides on `certifies` and lands with it.

**The default side needs no new mechanism.** `[test].default_profile` already
exists and already decides what bare `brokkr check` runs; pointing it at the
partial loop profile is configuration, not a feature. In particular there is
no magic `[profiles.default]` name: a config that defines a profile named
`default` today *without* setting `default_profile` runs every `[[check]]`
entry, so a magic name would silently change that config's behaviour -
a direct non-goal violation - while adding a second mechanism for a question
`default_profile` already answers.

**The gate side gets one key and one flag:**

```toml
[test]
default_profile = "edit"        # bare `brokkr check`: the loop answer
gate_profile    = "pre-commit"  # `brokkr check --gate`: the gate
```

`--gate` resolves through `gate_profile`, so `AGENTS.md` and pre-commit hooks
say "run `brokkr check --gate`" and stay correct through any profile renaming;
a profile name typed from memory into three documents is a name that gets
typo'd or copied stale. `--profile` survives for everything else.

Three load-time rules, in the same style as the existing `default_profile`
typo check: `gate_profile` must name an existing profile; that profile must be
`certifies = "complete"` (a gate that resolves to a partial profile is the
central lie this design exists to prevent); and `--gate` with no
`gate_profile` set, or combined with `--profile`, is an error.

Flag-name note: ratatoskr's `sync-bench --gate <name>` already exists and is
untouched - different subcommand, no clap conflict, and "gate" means the same
thing in both places. The overlap is deliberate, not an accident to fix.

## Build order

1. **Free today.** Rewrite nautilus's stale `sim` comment and land the sweep as
   seven `[[check]]` entries. Feature 5 already shipped; no brokkr change
   needed.
2. **Feature 8** (result contract). Small, unblocks honest reporting for
   everything after it, and removes the stdout-grep dependency immediately.
3. **`certifies`** plus **feature 1** (`skip_phases`) plus **feature 9**
   (`gate_profile` / `--gate`). The permission spine and its invocation
   surface; `--gate`'s complete-only rule needs `certifies` to exist. Until
   feature 4 lands, `complete` rejects crudely: any `skip`, `only`,
   `doctests = false`, or `include_ignored = false` anywhere in the profile is
   a config error, loosened to *accounted* forms when the accounting arrives.
   Shipping an unauditable `complete` under the flagship name, even briefly,
   would be this design's own sin.
4. **Feature 2** (`lanes`). Do it before feature 4: "it runs elsewhere"
   presupposes composed lanes.
5. **Feature 4** (coverage accounting, including doctests). The centrepiece -
   this is what makes `complete` mean anything.
6. **Feature 6** (conditional sweeps). Discharges problem 2; builds the
   changed-files -> packages mapping under additive semantics.
7. **Feature 3** (`scope = "changed"`), consuming that mapping. Last, because
   it is the only subtractive one, with the exit-status split present from its
   first commit rather than retrofitted.
8. **Feature 7** (slow-test budget). Independent; can land any time after
   feature 4 establishes the quarantine shape.

The order is really a commitment and an option. Steps 1-5 are committed: the
honest core, each justified on today's evidence. Steps 6-8 are re-evaluated
once the core lands - feature 6 against how often the sim sweep is actually
invoked deliberately (`when = "manual"` plus `--with` may be enough), feature
3 against measured `edit`-plus-manual-`-p` loop times, feature 7 against the
cost of its baseline store. Pricing the two halves separately now is cheaper
than discovering the difference later.

## Non-goals

- Changing the meaning of any existing key, or the behaviour of a config that
  does not opt in. This is a nautilus-scale problem; brokkr's other projects
  must see no difference.
- Making `brokkr clippy` collapse its output. It is the investigative runner and
  always prints its full command.
- Fixing the tests themselves. Problem 5 is upstream's project as much as ours;
  feature 7 exists to stop the next one appearing, not to fix the current ones.
