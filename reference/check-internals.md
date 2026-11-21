# Check and test internals

This page explains how `brokkr check` and `brokkr test` are built, and why,
so you can change the pipeline safely. Usage lives in `docs/commands/check.md`
and its siblings `check.test-lanes.md`, `check.hygiene.md` and `test.md`. The
config schema lives in `docs/brokkr.toml.md`, `docs/brokkr.toml.conventions.md`
and `docs/brokkr.toml.sweeps.md`. The sections cover, in order:

- **Invocation selection**: which lanes each phase attempts
- **Time ceilings**: the per-test cap and the clocks behind it
- **Strays** and **The rustc guard**: process hygiene around the lock
- **The lock queue**: how waiters are recorded and cancelled
- **Platform tunables index**: where the constants live
- **Clippy and rustdoc phases**: unexplained failures and lint-only rustdoc
- **The test phase**: lint injection, the failure list, lanes, the plan and
  execution accounting
- **Parallel allocator rationale**: why the allocator weighs what it does

## Invocation selection

brokkr decides once, before any phase runs, which lanes each phase of `check`
(and `brokkr test`) may attempt, and with which packages
(`src/check_cmd/selection.rs`). Every consumer reads that answer and nothing
re-derives it. The consumers are:

- clippy and rustdoc
- the test phase and `prepare`
- the lane runners: serial, parallel, isolated and nextest
- member-check, `brokkr clippy` and `brokkr test`
- the `package rules` and `brokkr test` plan announcements

For `check`, selection is built inside `plan_check`
(`src/check_cmd/check_plan.rs`). That is the one planning prefix that
`cmd_check` executes and `check --audit` renders. Planning's announcements go
through a `Narrator`: a run prints them as they are decided, and the audit
collects them. A refusal mid-plan therefore leaves the same partial narration
in both.

Selection decides invocations only. It never predicts binaries, tests,
inventory or outcomes; the prepared inventory and the runtime record (journal,
accounting) keep their own models.

### Lanes and their index

Each phase has one entry per lane it covers, in lane order. A `check` run's
lanes are a `RunSweeps`: the tested sweeps (configured, then derived doctest
carriers), then the owed `[[lint_sweep]]` lanes. Clippy and rustdoc select
over all of them; the test phase selects over the tested prefix only. So a
`LaneIdx` names the same sweep in every phase that has it.

The test machinery gets the tested slice and can't see a lint lane. That
machinery is `prepare`, the lanes, the coverage audit, the doc-only rules and
carrier derivation. Lint lanes are planned from the test-free `[[check]]`
entry of their shape after their cadence is decided. An entry the run doesn't
owe is therefore never planned and can't refuse the invocation. The carrier
deriver reserves lint-lane labels as a separate input.

The selection and prepared-lane collections are `Lanes<T>` values, built from
the lanes their phase covers and addressed with `LaneIdx`. A consumer pairing
selections of different phases (`feature_drop_lines`) takes each with its own
lane slice. Persisted lane records keep that same typed index in the journal
and termination labels.

### Selection, resolutions and disposition

Each lane entry records three facts:

- **Selection**: the effective package selection. It is one of the sweep's
  own `packages` list; `--workspace --exclude` from `test_exclude_packages`
  (test phase only, since the exclusions never narrow clippy or rustdoc); no
  package flags at all (cargo's default selection); or an override. An
  override is the CLI `-p` set (`check`, `brokkr clippy`) or `brokkr test`'s
  resolved package (from `-p`, `[test] default_package` or the project
  default), holding the packages the sweep's rules admitted. One function of
  the selection spells the package flags.
- **Resolutions**: one combined resolution, or one per package under
  `feature_unification = "package"`. Package mode always yields exactly the
  selection's packages, in order, each once. Package mode over a selection
  that names no packages is a configuration error raised before anything runs
  (no `--json` trailer). brokkr never repairs it into one unscoped resolution.
- **Disposition**: *eligible* (an attempt); *excluded* (the rules admit none
  of the `-p` packages); *deduped* onto an earlier eligible sweep of the same
  phase; *not applicable* (rustdoc's `doc_only` doctest carriers); or
  *disabled*. A phase is disabled by a profile's `skip_phases`, the
  markdown-only shortcut, or, for rustdoc, a missing `[rustdoc]` table.
  Nothing is computed or validated for a disabled phase. Admission is exactly
  the package rules above, with the exclusion list checked before the
  packages list.

### Cargo requests

Each eligible lane also has its cargo **requests**, built with the selection
and stored on it. There is one per resolution: its package selection plus the
sweep's features projected onto it (see
[Feature projection](../docs/commands/check.md#feature-projection)). There is
one more per `build_packages` support build.

Every cargo argv takes its package and feature flags from a request, never
from the sweep. That covers clippy, rustdoc, the pre-builds, the test runs,
the prebuilds and listings, the coverage enumeration and `brokkr test`.

The parallel lane's repro line for a failing binary is the whole request that
built it, its full package selection and projected features, plus the
binary's target selector. The repro therefore resolves the prebuild's exact
graph. Under a multi-package selection, a `--lib` repro runs every selected
package's lib harness.

### Dedupe

Dedupe differs by phase:

- **Clippy and rustdoc** fold a sweep onto an earlier one that compiles the
  same thing. That means the same compile inputs (profile, rustflags, env,
  unification) and the same requests: every run's selection (provenance
  ignored), resolution boundary and projected features, and every support
  build with its projected features. Two sweeps that a `-p` narrows to the
  same package and projected features are linted once.
- **`check`'s test lanes** never dedupe: their filters and execution policies
  differ even where their builds don't.
- **`brokkr test`** dedupes on what a lane would execute: the same requests
  and compile inputs, with the effective debug answer in place of the
  profile, plus `doc_only`. The debug answer is `--debug`/`--release` over
  the sweep's pin over `[test] debug`. A sweep and its doctest twin stay two
  runs, and two sweeps narrowed to the same package with the same build fold
  into one.

The same per-sweep answer can instead be a profile conflict: a flag
contradicting a `profile_required` pin (`build_purpose::eligible_sweep_profile`).
That lane is `Excluded` with a profile cause after the package rules and
before any request or dedupe candidate is built. It neither absorbs an
eligible twin nor folds onto one. The journal, built from attempted lanes
only, holds nothing for it.

The coverage audit still groups by the configured shape (`build_shape_key`).
The shape identifies the declared sweep, and a complete profile refuses `-p`
anyway.

### Derived doctest carriers

A complete claim without a `doctests` quarantine appends lanes before anything
is selected (`derive_doctest_carriers` in `selection.rs`, called from
`cmd_check`). Every phase, the reach ledger, the plan and the journal see them
as lanes like any other.

Each non-doc, non-curated lane whose resolutions reach a doctested member
(`DocFacts`, from `cargo metadata`) is keyed on its test-phase effective build
with support builds included. That key is the clippy key plus
`build_packages`, since a doctest can run a support binary through
`BROKKR_TEST_BIN_DIR`. The lane gets a carrier unless a configured `doc_only`
lane or an earlier carrier holds that key.

The carrier is a clone of the planned sweep (`PlannedSweep::doctest_carrier`),
typed by `SweepOrigin::DoctestCarrier`:

- doc-only execution under the watchdog
- every libtest filter dropped
- `curated` off
- the invocation's unification answer kept rather than re-resolved

A complete claim refuses `-p` and forwarded args before selection, so the
clone's attempt builds exactly its source's requests. Dedupe is per lane, not
per resolution. A configured `doc_only` entry equal to only some resolutions
of a lane leaves the whole lane's carrier in place: a redundant pass rather
than a narrowed request built outside the selection.

Carriers are `NotApplicable` in clippy and rustdoc, since the source lane
lints the build. A package-mode carrier executes only its resolutions that owe
doctests, because `cargo test --doc` over a bin-only package fails before any
rustdoc runs.

### Lanes that are not eligible

A lane that isn't eligible builds nothing for that phase. In particular,
`prepare` records an excluded test lane as skipped and builds neither its
support binaries nor its shape. An eligible lane keeps the order: support
builds, then its own build and listing, then the support fingerprint. Support
builds always use their own single-package selection (each `build_packages`
entry), never the lane's test selection.

### Nothing reached

When the CLI `-p` set excludes every applicable sweep of an enabled phase,
that phase refuses. The refusal is decided with the selection and reported at
the phase boundary as a failed run, with `failed_phase` set and the `--json`
trailer emitted. Clippy and rustdoc report it as they start:

```text
-p x: every sweep's config rules the selection out (a, b); nothing reached clippy
```

The test phase's refusal is reported **before** `prepare`, under
`failed_phase: "test"`, so a run that will test nothing compiles nothing for
it. That refusal therefore takes precedence over any preparation,
`cargo metadata` or plan-persistence error the run would otherwise have hit.
Its text is:

```text
-p x: every sweep's config rules the selection out; zero tests ran
```

A disabled phase never refuses. Nor does rustdoc when every sweep is a
doctest carrier.

### Reached sweeps

The trailer's `sweeps` list is the union of two per-sweep flags: one for the
diagnostic phases (clippy and rustdoc share it) and one for the test phase.
brokkr sets each flag when the phase starts work on that sweep, before cargo
returns. A sweep whose run failed still counts, and a lane an earlier
fail-fast never reached doesn't.

### Member-check and fixture-lint selection

Member-check has its own selection. It's not applicable when there is no
`[member_check]` and no `[bin] install` implying one, when the cadence rules
the claim out, or when the phase is skipped. Otherwise it's eligible with the
`-p` set.

Which members exist is a `cargo metadata` fact, so brokkr resolves the member
set when the phase runs: workspace members less `skip`, intersected with `-p`,
skipped visibly when that empties it. Which bins are eligible stays cargo's
runtime answer. Fixture-lint's selection is the entries whose `when` admits
the claim, independent of `-p`.

## Time ceilings

This section covers how the per-test cap and its sibling clocks are enforced,
what they can prove, and where each bound is defined. Usage is in
`docs/commands/check.hygiene.md`.

### Deadline precision

Three details decide whether "20 seconds" means 20 seconds:

- **The watchdog wakes on the deadline**, not on a fixed interval. It sleeps
  to the soonest thing it bounds: the oldest in-flight test's ceiling, the
  no-progress clock, the idle window or the caller's wall. Its poll caps the
  sleep, so a test that starts mid-sleep has its deadline read promptly. A
  flat 250 ms poll would make every cap "the ceiling plus up to 250 ms".
- **The end of a run is not a poll away.** The runner waits on the child's
  pidfd, so the child's exit wakes the waiter at once; the poll only bounds
  how late a stop or a sibling's abort is noticed. The watchdog sleeps on a
  doorbell (a condvar) that the end of the run rings, so joining it doesn't
  wait out its sleep either. With flat sleeps, a suite of sub-millisecond
  tests would cost up to two polls per run.
- **The run is frozen before it is diagnosed.** brokkr SIGSTOPs the offending
  group first, then snapshots it, then SIGKILLs it. Snapshotting first would
  let the test keep running while `/proc` collection took. Killing first
  would fix the cap but destroy the diagnostic, since a dead process has no
  `wchan` and no stack. SIGSTOP stops execution immediately and leaves
  `/proc` readable.

The shared-stream, shim and parallel libtest watchdogs use one deadline
verdict and one freeze, snapshot and kill sequence. The verdict prefers
per-test expiry, then idle expiry, then wall expiry. The shim measures idle
time across the whole session, including gaps between harnesses. Parallel
wall expiry has the same snapshot and in-flight context as serial wall
expiry. The shim also honors the caller's `WallShape`: a resolved one-test
invocation uses the caller's name, and a shared wall expiry lists in-flight
names as context only.

### What the cap can prove

The cap is enforced from a clock fed by libtest's *announcements*, and those
share one stdout stream with whatever the tests write to it. A test can
bypass libtest's capture with ordinary safe Rust,
`write!(std::io::stdout(), "x")`, because libtest installs Rust's capture
mechanism rather than redirecting the process's OS descriptor. A subprocess
started with `Command::status()` inherits the descriptor outright. Output with
no trailing newline gets libtest's next record concatenated onto its line.
brokkr recovers that case (see `split_trailing_event`), and a record that
still goes unparsed is a lost event.

Those records also *refresh* the clocks, so a test that emits them could
otherwise buy itself unlimited time: re-emitting one completion on a timer
would reset the no-progress clock forever. brokkr therefore counts a refresh
**once per test name**. A forger can spend only as many refreshes as distinct
names it invents, and a replayed record buys nothing.

The residual limit is this: on a shared harness, the cap is enforced through a
stream the tests can write to. A determined test that invents a fresh
plausible name every twenty seconds is bounded by the sweep backstop, not by
twenty seconds. An unforgeable per-test cap needs the process to be the unit
of one test, which the isolated lane, the nextest lane and
`brokkr test --timeout` give you.

That limit affects the **name**, not the entitlement to kill. If a test's
terminal event is lost, either that test really has burned its budget, or
brokkr has seen no test complete for longer than the budget. Either way the
run has exceeded what the contract allows, so the cap fires regardless. When
brokkr knows its attribution is a guess, the report says so rather than
presenting a suspect as proven.

Where the process is the unit of exactly one test (`brokkr test --timeout`,
the isolated lane, the nextest lane), the name is the caller's selection
rather than anything parsed, and the attribution is exact as well.

### Missing evidence fails closed

Every verdict path fails closed on missing evidence, because a zero and a
silence look identical in a counter. A `cargo test` that exits 0 having
reported nothing isn't a green run; it's a run brokkr can't vouch for. The
shapes that reach it are ordinary:

- a test that prints and terminates the harness
- an abnormal exit after a suite announced itself
- a custom harness that ignores the flags

So `ParsedTestResults` has a `Completeness`, set by comparing suites that
announced themselves against suites that summarised. They must agree exactly,
and zero of each is `Incomplete` rather than `Complete`: silence isn't
agreement. Every consumer checks it before reading a count as a fact:
`brokkr test`, the `check` sweep report, the isolated lane and the parallel
lane. The parallel lane also can't infer a pass from a clean exit, because it
derives its count from observed completion events.

The same rule governs enumeration. A listing with no `N tests, M benchmarks`
tally isn't an empty listing, and a cargo artifact stream with no
`build-finished` record isn't an empty selection. Both fail rather than
contribute an empty set, because the coverage audit certifies over exactly
those sets. An audit that attests to nothing while printing
`0 pairs, 0 orphaned` is worse than one that refuses, since a green audit is
taken as evidence.

### Clocks per lane

`Ceilings` in `src/test_runner/deadline.rs` holds the cap, an optional wall
backstop, and a `WallShape` saying what a kill may be *called*. The numeric
bounds live in `src/test_runner/ceilings.rs`. Every lane enforces the same
20s cap; lanes differ only in how exactly a kill can be attributed:

| lane | cap | kill named by |
|---|---|---|
| `check` serial sweep | 20s | tracker's best-known suspect |
| `check` parallel sweep | 20s | tracker's best-known suspect |
| `brokkr test NAME` | 20s | tracker's best-known suspect |
| `brokkr test --timeout N` | N, max 280s | caller's resolved test name |
| isolated lane | 20s | caller's selected test name |
| nextest lane | 20s (engine `slow-timeout`, `terminate-after = 1`) | engine, process-per-test |

Two further clocks bound what the per-test cap can't charge to any test:

- **Idle ceiling** (5 min, `IDLE_TIMEOUT`): no test is in flight at all, so
  there is no budget to spend. The motivating case is cargo parked on
  `Blocking waiting for file lock on build directory` because another cargo
  held the target lock, with no test ever announced.
- **Wall backstop** (30 min, `SWEEP_WALL_TIMEOUT`): a wedge neither clock
  above can attribute. The 15 min `test` phase ceiling always fires first, in
  `check` and in `brokkr test` per sweep and per iteration, so this bound
  applies only to a caller that arms no phase watchdog.

One more bound applies after the clocks stop: **output drain grace** (2s,
`DRAIN_GRACE`). The cargo streams and each isolated harness stream use the
same join policy. After cargo, or a directly run test binary, exits, the
drains get that long to reach EOF. A process the tests left running that
inherited a pipe can keep it open indefinitely. Past the grace, brokkr
cancels and joins the drains, warns that capture stopped, and goes on with
what arrived.

brokkr signals nothing at that point: the run's process group may already be
gone and its id reused. A leaked process with the run token is reaped at the
next hold (see "Orphaned test processes").

`src/test_runner/ceilings.rs` indexes the time limits. `Phase::ceiling`
selects the phase ceilings, and the check watchdog applies the whole-run
bound. Output drain grace stays in `src/test_runner/procgroup.rs`. No config
key or flag sets these limits.

## Strays

The ceilings bound the damage; the stray reap removes the cause. brokkr's
premise is that every cargo invocation on a development host goes through it.
The global lock serializes on that premise, and the measurement stores assume
it. A cargo running outside brokkr competes for CPU with whatever is being
measured, and takes cargo's build-directory lock, on which brokkr's cargo then
blocks with nothing to time out.

Both hangs the idle ceiling and the reap answer have been seen in practice. A
rust-analyzer `cargo check` (a nightly regression) held the target lock for
over an hour, and four of its build scripts later spun at 100% CPU under it.

A fresh locked hold reaps unowned cargo-family processes after acquisition:

1. `src/stray.rs::is_cargo_family` classifies the executable.
2. The reaper applies its process-identity and ownership checks.
3. It kills leaves first.
4. It reports comm counts plus deduplicated starters on one `[strays]` line.

The live per-process detail is `brokkr strays`' job. The starter is the
nearest ancestor outside the family. When the starter is rust-analyzer, the
reaper kills it too, because it would only restart the cargo. A shell or
editor starter (a hand-typed `cargo`) is never signalled. A failure to read
`/proc` reads as nothing found: the reap is on the way to the real work,
never a gate on it.

Formatters are exempt: `cargo-fmt`, and a `cargo` whose first positional
argument is `fmt`. `brokkr fmt` holds no lock unless `disable_toolchain` is
set, so its cargo has no verified owner; without the exemption, every locked
command elsewhere on the host would SIGKILL it. `cargo fmt` takes no
build-directory lock and compiles nothing, so it is outside what the reap
exists to stop.

### Process-tree boundary

**Both reapers spare the same process-tree boundary: this brokkr, its visible
ancestors, and its visible descendants.** `ReaperExemptions` in
`src/stray.rs` defines it, and the orphan selector uses it too. A
token-bearing ancestor of a live brokkr, and a cargo process that launched it,
survive either reap.

An ancestor cargo can hold the target-dir lock. Killing it could release that
lock for a nested cargo, and would also SIGKILL the cargo operation that
launched brokkr. The reapers preserve that caller, so a recursive cargo under
a build script can stay blocked by its caller's target-dir lock.

### Verified ownership

**Stray ownership adds verified identities, never a name.** Exempting any
process with a `brokkr` ancestor by `comm` would be spoofable by naming *any*
executable `brokkr`: a shell copied to that name would exempt every cargo and
rustc beneath it, whether or not it held anything. Beyond the shared boundary,
the stray reaper recognizes **the verified current lock holder** (checked
through the lock file's starttime and boot-id tokens) and its owned subtree.

Every verification by PID first asks whether this `/proc` numbers processes
for the caller's own PID namespace (`process::procfs_is_own_namespace`). That
covers this one, `brokkr kill`'s numeric path, and `brokkr lock`'s fallback
and PID column. The test is that `NSpid` in `/proc/self/status` holds exactly
one entry, our own PID.

We don't compare `/proc/1/ns/pid` with our namespace instead. Reading init's
namespace link needs ptrace access to it, so for every non-root user the
comparison fails and no holder ever verifies. PIDs would never show, the
fallback and `kill --hard` would refuse, and a hand-run `brokkr strays` would
not protect the holder's subtree.

### Holders in another PID namespace

A holder in **another PID namespace** (a sandboxed command, seen from the
host) can't be verified by PID at all, so its namespace stands in for it.
While it holds, every process in that namespace or nested below it is treated
as its own (an `NS_GET_PARENT` walk; a process that can't be inspected is
exempted). This applies only when the holder's namespace differs from the
reaper's, since for a holder sharing it the rule would exempt every process
on the host. It guards every target the reap would signal, descendants and
starters included. Without it, a hand-run `brokkr strays` on the host would
kill a sandboxed holder's own cargo.

There is no name fallback. The case that argued for one, never SIGKILLing
brokkr's own build, is covered by the shared boundary, and the reaper can
identify its own process by PID. A hold whose identity can't be verified from
this namespace therefore protects nothing beyond that boundary. A name
fallback would let an executable called `brokkr` shield a foreign compiler
from a drain that then expires for want of killing it.

### Descendant trees

**The reap kills descendant trees, deepest first, and stops at exempt or
owned processes.** Signalling only the classified PIDs would leave a hole:
killing a cargo doesn't kill the rustc wrapper beneath it, so an
already-admitted wrapper would survive and go on to exec a compiler. The
shared boundary prevents selecting the cargo that launched this brokkr, and
stops expansion at brokkr's descendants. A hand-run reap can still meet
another verified holder below a stray cargo; the reap skips that holder and
its subtree and doesn't recurse through them.

### Identity-checked signals

**Every signal is identity-checked.** A `Stray` keeps the `/proc` starttime
recorded at classification, and brokkr sends the SIGKILL only if that PID
still has that starttime. A bare PID isn't an identity: between the scan and
the signal the process can exit and the number be reused, and the reaper
would then kill something it never classified. The lock file pairs every
recorded PID with its starttime and the holder's boot id for the same reason.

### Hard kill verification

`brokkr kill --hard` verifies every recorded target before the first signal,
and refuses the whole kill on one it can't verify. The one exception is a
recorded child or mock with positive evidence that its generation is gone,
which brokkr skips with a line saying so. Positive evidence needs all of:

- its PID's stat shows a different starttime, or `pidfd_open` reports no such
  PID
- the record's namespace tokens match
- the record's boot id is this kernel's
- the holder itself is verified

A missing `/proc` entry alone isn't evidence, since `hidepid` hides live
processes. The holder's own slot writers prune by the same positive rule,
never by a zombie state, since a zombie thread-group leader can still have
running threads.

A process-group leader is re-verified and then signalled by `kill(-pgid)`,
since a group has no pidfd. The kernel keeps a PID number allocated while any
task uses it as a pgid. The residual sliver needs the whole group gone and a
recycled PID that became a group leader in between, and we accept it.

Containment is narrowed, not closed. A process can fork again between the
`/proc` read and the signal, and a lease holder can be a detached process
with a name no cargo-family scan matches. Closing that needs a cgroup, which
is a larger change than this reaper.

### Where the reap runs

The reap, and the stale-guard check below, run inside `lockfile::acquire`
itself, once per *fresh* hold. A re-entrant acquire never reaps, since its
outer hold already did. The reap lives in `acquire` rather than in a command
wrapper so that paths taking the lock directly (bench contexts, piners,
ratatoskr) can't skip it.

The post-acquisition reap runs only once the hold is usable, after the drain,
so it can't rescue an acquisition that is itself blocked. That is why the
drain has its own reap on stall (see "The compile lease").

`brokkr strays` is the by-hand form: bare, it lists; with `--kill`, it lists
then kills. It works with no `brokkr.toml`. A legitimate run that needs longer
is a run whose sweeps want splitting, not a run that wants a longer rope.

### Orphaned test processes

The stray reap matches by name, and a test binary is named after its crate. So
a second reap (`src/test_orphans.rs`) runs beside it at every fresh hold, for
the one shape a name can't find: test processes left by a brokkr that died
without running a handler, SIGKILLed by the OOM killer for instance. The
runners' parent-death signal takes their direct child with brokkr. On the
serial lanes that child is cargo, though, and the test binary beneath it (or a
doctest under rustdoc, or anything a test spawned) is reparented and keeps
running.

Every process a test runner starts has `BROKKR_TEST_RUN=TOKEN`, one random
token per brokkr process, and inheritance passes it to everything below. That
covers libtest and nextest lanes alike, and every child that inherits a lease:
a reserved launch, or a script-bench call's borrow. `src/reservations/` also
gives such a child the parent-death signal.

The same brokkr process holds an exclusive flock on
`~/.brokkr/test-runs/TOKEN.lock` for its whole life. The flock is the liveness
proof. The kernel drops it however the process dies, and it reads the same
from every PID namespace sharing the filesystem, where a recorded PID would
mean something else.

The reap works in four steps:

1. It takes each file's flock it can get, which means a dead owner's.
2. It SIGKILLs every process whose `/proc/PID/environ` has that token, newest
   first, each checked against its starttime. A child starts after its
   parent, so newest first is leaves first.
3. It rescans, and deletes the file once no carrier is left.
4. A file whose carriers survive stays for the next reap.

A brokkr that exits normally leaves its file too. The next reap finds no
carrier and deletes it, and a process a test leaked past a clean exit goes the
same way as one left by a crash. The reap prints one line when it kills
anything:

```text
SIGKILL sent to 2 orphaned test processes (my_crate-3f2a, sh)
that outlived the brokkr run that started them
```

Every failure leaves processes alone: an unreadable directory, a missing
file, a flock held by anyone, or an environment that can't be read. An
unreadable environment is another user's process, or a test process that made
itself non-dumpable, which the reap therefore misses rather than kills. The
reaping brokkr uses the same `ReaperExemptions` boundary as stray cleanup, and
spares itself, its ancestors and its descendants whatever token they carry.

The mark is inherited, so a brokkr started from inside a test has the outer
run's token, as does every non-test child it starts. Killing happens after
the fact, not when brokkr dies. An orphan runs until the next locked brokkr
command, which is also when the stray reap clears the rustc and rustdoc left
under the dead cargo. `brokkr strays` doesn't list orphans; any locked brokkr
command reaps them. Unit tests never register a token or run the host reap;
they exercise the mechanism against a scratch directory and a child the test
owns.

### Launch token

A second inherited mark sits beside `BROKKR_TEST_RUN` on every child a hold
spawns: `BROKKR_EXEC=ACQUISITION.SEQUENCE`. `process::spawn::spawn` allocates
and stamps it, as does the nextest engine's generated config once per lane.
Both take it from the same `LockInner` snapshot as the capability, so a child
never has one acquisition's authorization and another's cohort. `brokkr lock`
uses it to find the holder's processes wherever they reparented
(`src/lock/collector.rs`).

The launch token is for reporting only, and inert everywhere else:

- neither reaper reads it
- the watchdog and `kill --hard` don't select by it
- the guard admits by capability, not by it

A process with one mark and not the other is reported or reaped by whichever
mark it has, no more. Its semantics are those of `environ`, the environment
of the last exec, so a process that changed its environment in place is
attributed by what it was exec'd with.

## The rustc guard

The reap removes strays after the fact; the guard stops them compiling at
all. `brokkr-rustc-guard` (`src/bin/rustc_guard.rs`) is a second bin target
installed next to `brokkr`. `brokkr guard --install` enrolls it user-wide as
`build.rustc-wrapper` in `$CARGO_HOME/config.toml`. From then on every cargo
this user runs routes each rustc invocation through the guard: PATH-resolved
or absolute-path, rust-analyzer's, an agent harness's, or a hand-typed one.

Cargo's startup `rustc -vV` probe goes through the wrapper too. The guard
admits probes (see "Query admission"), since refusing one poisons cargo's
probe cache. A refused build therefore dies at its first real compile: zero
crates completed, one refused.

### Admission capability

Admission is a capability for one acquisition, **not** an inference about the
process tree. We don't admit on a `brokkr` ancestor in `/proc`, for two
reasons:

- It's spoofable: any executable *named* `brokkr` would admit every compiler
  beneath it, and a shell copied to that name is enough.
- It infers from process *shape*, so it fails wherever shape isn't visible: a
  PID namespace, a restricted `/proc`, a child that reparents. Its failure
  direction is the worst available, refusing the one process that must be
  allowed, because for brokkr's own builds brokkr *is* the holder.

The comm walk isn't kept as a fallback either.

Instead, the holder marks its descendants. When a hold begins, brokkr mints a
random per-acquisition **nonce** (`src/hold.rs`) and hands it to every child
it starts, through the environment. Inheritance is the point: the nonce
reaches a grandchild cargo spawned by a test binary or a script check, without
brokkr having to predict which executables might invoke cargo. The lock file
holds only the nonce's *hash*, so reading the world-readable lock file
doesn't hand a bystander a working capability.

The nonce is per **acquisition**, not per process. An identity built from
PID, starttime and boot id names the process. One long-lived brokkr could take
hold A, spawn a child, release A and later take hold B, and the child's mark
would still match: it would be admitted into a window it was never authorized
for. A nested (re-entrant) acquire shares the nonce; a new hold in the same
process mints a new one.

### The compile lease

Admission alone isn't enough: a guard can be stopped between admission and
`exec`, then start a compiler inside a much later hold. So admitted
compilation holds a **reader lease**, and a brokkr acquisition is the
**writer**, on a second lock file, `~/.brokkr/compile.lock`.

The guard takes a *shared* flock on `compile.lock`, clears `FD_CLOEXEC` so
the lock survives `exec`, and only then reads the published hash. Lock first,
then check: a new hold can't republish the capability without the exclusive
lock that the share excludes, so the value read can't change underneath.
flock records live on the open file description, so the compiler itself holds
the lease, and the kernel releases it when the last holder of that
description exits. A crash leaves nothing to clean up.

Acquisition does this before the hold is usable:

1. Publish identity with `auth=` **empty** and `draining=1`. A guard reading
   an empty `auth` refuses, so nothing new is admitted while brokkr clears.
   That is the anti-starvation property, obtained from the publication order
   rather than from a separate intent flag a crashed writer could leave
   behind.
2. Take the **exclusive** `compile.lock`, draining earlier leases. Those
   belong to compilers admitted under an earlier hold, and to compilers
   started while no hold existed (an IDE check, a hand-typed `cargo`). After
   a short initial window the stray reap runs once, then the wait resumes
   under the configured drain budget. The window is short because every
   compiler still holding a lease at that point is a stray the post-hold reap
   would kill anyway; waiting longer only stalls the acquisition.
3. Mint the nonce and publish its hash *while still holding the exclusive
   lease*. A guard paused before taking its share then resumes, reads the new
   hash under its share, and can't turn an earlier hold's nonce into an
   admitted compiler.
4. Release the exclusive lease so the hold's own descendants can take shares.

`compile.lock` is **never unlinked or replaced**. The proof that a drain
succeeded is an exclusive flock on that *inode*, and a fresh inode at the same
path would silently split the exclusion in two.

**Acquisition can fail.** If the budget expires, brokkr releases
`brokkr.lock` and reports that compilation leases didn't drain; it doesn't
proceed into measurement. This isn't a transient busy signal. A lease lives as
long as *any* process keeps the descriptor, and a detached helper can extend
it without bound under a name no cargo-family scan matches, so an unchanged
blocker fails every retry.

### The decision order

The guard decides in this order:

1. `BROKKR_CARGO` set passes, checked **before any lease**. A hatch that needs
   working lock infrastructure is useless exactly when it is needed.
2. The guard takes the lease and reads the record.
3. A flock probe of `brokkr.lock` has the final word on whether a hold exists
   at all. Not held passes, whatever the file says, and the guard keeps the
   shared lease through `exec` so a later hold drains this compilation.
4. Held with an empty `auth` refuses: a hold is starting up.
5. Held with a non-empty `auth` admits only a matching capability.

The flock probe is last on purpose. Deciding from the record alone is a bug
that a probe run found. Abnormal termination leaves metadata behind while the
kernel releases the flock, so a crashed brokkr publishes a well-formed
capability that matches nobody. Refusing on it refused every compile on the
machine until something rewrote the file. A leftover record is inert; only a
live flock is a hold. Only `EWOULDBLOCK` demonstrates a holder; `EINTR`,
`ENOLCK` and the like say nothing and fail open.

The two sides fail in **opposite directions**, deliberately, because their
contracts differ:

- **The holder** fails acquisition if it can't take the exclusive lease or
  publish a complete record. brokkr must never run protected work believing it
  has exclusion it never obtained.
- **The guard** fails **open** when the lease infrastructure itself is broken:
  `compile.lock` unopenable, `ENOLCK`, a read-only `$HOME`, or `FD_CLOEXEC`
  unclearable. It says so on stderr. Failing closed there would let brokkr
  brick every build on the machine.

So the guarantee is **conditional**, and it is stated precisely:

> Protected work excludes compilation that successfully participates in the
> lease protocol. Explicit overrides and guard infrastructure failures can
> overlap protected work.

A sandboxed guard denied access to `compile.lock` fails open and compiles
straight through a measurement, with no race required. brokkr can't even
reliably report that the guarantee degraded, since the failing guard may be
elsewhere. The reap stays the backstop for non-participating cargo, and for
non-compiling cargo (`cargo metadata` isn't fenced and doesn't need to be).

### Query admission

The guard admits compiler-information queries unconditionally, before asking
any lease or lock question (`classify` in `src/bin/rustc_guard.rs`). Queries
are `rustc -vV` and the `--print` batteries cargo sends when probing a
toolchain. Two reasons, and the second is the one that forced the rule:

- **A query compiles nothing.** The classifier parses the invocation's option
  grammar and accepts only shapes rustc answers before expansion and codegen:
  version requests, and `--print` requests from an explicit early-print
  allowlist, with no input beyond stdin's `-`. `--print=link-args` and
  `--print=native-static-libs` are compiles wearing a query's hat (rustc
  compiles first to answer them) and stay gated, as does any option or print
  name the parser doesn't know. Misreading a query as a compile costs one
  refusal. The reverse would run real work outside the exclusion guarantee,
  so unknown shapes fail toward `Compile`.
- **Refusing a query poisons cargo's cache.** Cargo **persists a failed
  probe**, stderr and all, in `target/.rustc_info.json`, keyed by a
  fingerprint that is blind to inherited env vars, and **replays it without
  re-running the probe**. A foreign nonce-less cargo refused at its probe
  during anyone's hold would poison the shared cache. Every later cargo with
  the same fingerprint, brokkr's own correctly-stamped children included,
  would fail with the guard's refusal verbatim while the guard never runs.

Under that poison the nonce is present and useless, and `BROKKR_CARGO=1`
can't cure it, because it only reaches a guard that cargo no longer spawns.
The poison heals only when a concurrent cargo holding a pre-poison in-memory
copy rewrites the file (last writer wins), which makes the failure
intermittent. It has been observed live: a piners `cargo doc` script check
failed two runs with "carries no brokkr capability" despite a
correctly-stamped child, then healed with no external change.

Query admission stops *new* poison at the source. Against *historical* and
*racing* poison, the shared spawn policy stamps `CARGO_CACHE_RUSTC_INFO=0`
(`hold::RUSTC_INFO_CACHE_ENV`), and nextest's generated config passes it to
engine-owned children. You cure a poisoned entry met *outside* brokkr with
this command, where `CARGO_ARGS` stands for the cargo command you were
running:

```sh
CARGO_CACHE_RUSTC_INFO=0 BROKKR_CARGO=1 cargo CARGO_ARGS
```

The first variable makes cargo actually probe; the second makes the guard
admit the probe's compile siblings.

### Reserved environment variables

`BROKKR_HOLD_NONCE`, `BROKKR_COMPILE_LEASE` and `CARGO_CACHE_RUSTC_INFO` are
brokkr's transport. Project configs, `[[check]] env` blocks, and
user-authored build scripts or shell commands must not set, unset or override
them. The internal guard decision probe sets `CARGO_CACHE_RUSTC_INFO=0` for
its isolated cargo build, because that direct invocation bypasses brokkr's
cargo spawn policy.

The one hole `force` can't close is a *workspace* crate's own `build.rs`
emitting `cargo::rustc-env=` for a reserved name. The nextest engine applies
the test package's build-script env after cargo's `[env]` table, so the
emitted value would displace the forced one for that crate's tests. The engine
applies build-script env only for the test binary's own package, so
dependencies can't cause it.

`BROKKR_CARGO=1` is different in kind: it's the deliberate human escape
hatch, set per invocation by a person and never by tooling.

### The version handshake

The guard answers `brokkr-rustc-guard --brokkr-guard-info` with a protocol
number. It recognizes the flag before any lease or lock access, so the answer
comes even when the lock infrastructure is wedged. brokkr compares it against
`guard::GUARD_PROTOCOL` at `brokkr guard` status, and once per locked command
after acquisition.

brokkr warns, and never fails, on a mismatch or on a guard that doesn't speak
the handshake. An old guard treats the flag as the executable to wrap and dies
at exec, which the bounded probe reads as stale. The two binaries install
together, but nothing forces them to stay in step, and a half-upgraded pair
fails in shapes that read as anything but staleness. The number names required
*behaviour*, not wire format: bump it whenever admission semantics change in a
way brokkr depends on.

### Recursive acquisition

A brokkr command started from *inside* an admitted compilation would take
`brokkr.lock`, then wait forever for an exclusive `compile.lock` that its own
waiting parent holds a share of. A proc macro that shells out to cargo is
ordinary Rust, so this case is real. The guard therefore marks the compiler it
admits (`BROKKR_COMPILE_LEASE`), and a brokkr that finds that mark refuses a
*fresh* hold, naming the deadlock.

Nested (re-entrant) acquisition in the same process is unaffected, since that
check comes first. Refusing loses that build; blocking loses the machine. The
mark is a convention, not a kernel fact. Scrubbing the environment drops it
while leaving the descriptor open, and a helper can keep it after closing its
own descriptor. Both fail toward a refusal rather than a hang.

### Descendants of the holder

The same shape one level up: a locked brokkr started *beneath* a command that
holds the lock would wait on the flock while its ancestor waits for it to
exit. The machine-wide lock then stays held until someone kills both. Examples
are `brokkr run brokkr -- test NAME`, and a script check or a test that shells
out to brokkr.

So a descendant of the holder refuses rather than waits. A waiting
acquisition compares the capability nonce it inherited (`crate::hold`; every
child of a hold has that hold's nonce) against the hash the holder publishes.
On a match it refuses at once, naming the holder and the remedy: run the inner
command outside the outer one. For brokkr itself, that means invoking the
built `target/debug/brokkr`, or an installed one, directly. Process ancestry
plays no part, so this works where a PID namespace hides the parent.

A match proves descent from the live hold, not that waiting would deadlock,
since an ancestor could release and only then wait. The refusal says what it
knows. The acquisition waits as usual for:

- an unrelated holder
- a later hold after the inherited one was released
- a holder still draining, which publishes no hash yet

The wait polls every 100 ms rather than blocking in `flock`. A record that is
momentarily unreadable while the holder rewrites it then can't commit the
waiter to an uninspectable wait, and a shutdown requested while waiting ends
it. Same-process re-entry is checked first and is unaffected.

`brokkr run` keeps holding the lock while its program runs: its serialization
against benchmarks and its `brokkr lock` and `kill` tracking are the point. A
locked brokkr can't run beneath it, and that is the case this refusal reports.

### Wrapper stays configured

The wrapper stays configured for brokkr's own builds too, and is never
bypassed by unsetting it. The wrapper's identity is part of cargo's compile
fingerprint (cargo #9348), so a wrapper that came and went per invocation
would ping-pong full rebuilds. Enrollment therefore costs one full rebuild
per target dir, once.

`[[script_check]]` children need no special treatment. A script check is
arbitrary shell that may run cargo (`cargo doc --no-deps` is the usual one),
and brokkr doesn't own the process tree below `sh -c`. That is exactly where
an ancestor walk fails: a `cargo doc` script check would be refused at its
`rustc -vV` probe. The inherited capability survives detaching, re-execing
and reparenting, so no hand-granted override is needed.

brokkr never grants `BROKKR_CARGO` to its own children. That would let a
script descendant keep compiling into later holds it was never authorized
for, which is the failure the per-acquisition nonce exists to prevent.

### The spawn choke point

`process::spawn::ChildPolicy` owns the shared spawn sequence, in this order:

1. the caller environment
2. `cargo_env`
3. OOM protection
4. test and service parent-death and orphan policy
5. the hold capability, last

The output runners use that policy, and its rendered environment feeds
nextest's generated config. Direct `Command` construction in other subsystems
is outside this choke point. Don't assume those paths carry the hold
capability or the rest of the policy.

### The nextest lane's capability

The nextest lane (`src/check_cmd/nextest_lane.rs`) launches test processes
through the linked engine's `TestList::new` and `runner.try_execute`. Those
construct their own commands, with no `Command::new` for the choke point to
catch. The capability goes by the engine's documented route instead. Each
`CargoConfigs` discovery owns a private, owner-only generated file with forced
`[env]` entries from the sweep and the shared child policy. The engine applies
them to every process it spawns, including the rustc-info cache disable.

brokkr removes the file after eager discovery, including on failure, and
concurrent callers without a hold can't replace another reader's environment.
The synthesized nextest profile uses the same private-file lifecycle.

It's a file rather than CLI `--config` expressions because the engine's CLI
parser accepts neither shape that can express `force`. Inline tables are
rejected as not-a-dotted-key. Split dotted keys fail because each argument
deserializes alone, leaving a `force` with no `value`.

### The guard command

`brokkr guard` bare shows status, including a warning when the configured
guard binary is missing. It works with no `brokkr.toml`. The flags:

- **`--install`**: writes the config line, resolving the guard as the binary
  next to the running `brokkr` executable. It refuses to overwrite a wrapper
  brokkr didn't write (sccache, say: chaining wrappers is a human's decision),
  and refuses to replace a guard from another brokkr install.
- **`--remove`**: can also unset another exact-name brokkr guard, including
  one whose install is gone, but still refuses an unrelated wrapper.

### The decision probe

`scripts/guard-decision-probe.py` builds this checkout's `brokkr-rustc-guard`
in a persistent `brokkr-guard-decision-probe` subdirectory of cargo's resolved
target directory. The build is keyed by checkout path and kept separate from
check and test artifacts. The probe covers the capability paths against a
scratch `$HOME`:

- an idle machine
- a stale record left by a crash
- a draining hold
- a mismatched capability
- no capability
- the `BROKKR_CARGO` hatch
- query admission against a refusing hold, with its compile-continuing-print
  counterexample

The `rustc-guard-decision-probe` script check runs it at the `pre-test`
stage, so it doesn't depend on the binary `brokkr install` installed. Run it
after any change to `decide()` or `classify()`: it caught the stale-record
refusal described in "The decision order". `brokkr check` covers the admitted
path end to end by compiling anything at all while brokkr holds its own lock;
if that path broke, no build on the machine would work.

## The lock queue

The lock queue (`src/lock/queue.rs`) records every blocking acquisition so
`brokkr kill` and `brokkr lock` can see it. A blocking acquisition joins the
queue before its first flock attempt, so every waiter is recorded. Each
record holds:

- its PID with starttime and boot
- its namespaces
- its project key, or that it has no `brokkr.toml`
- its state root
- its command line

The record is published as `~/.brokkr/queue/TOKEN.queued` under an exclusive
flock, which is its liveness proof, as for the test-run tokens. A join that
can't complete refuses the acquisition: a waiter never waits unrecorded, since
a kill could neither see nor cancel it.

The queue also decides admission order: waiters take the lock in arrival
order, through ordered tickets decided under `admit.lock`, with a heartbeat
that lets a stale place be passed. The `src/lock/` entries in
`reference/architecture.md` state that protocol.

### Cancellation

A cancel sends no signal. The handshake has three parts:

- `brokkr kill` renames `TOKEN.queued` to `TOKEN.cancel`.
- The waiter looks for its `.queued` name every poll round, and gives up
  (exit 130) when it is gone.
- The waiter that wins the flock claims its place by unlinking `.queued`.

A rename and an unlink of one name are atomic against each other, so exactly
one side wins, from any PID namespace that shares the filesystem. A waiter
that is stopped when its cancel commits can never run. Only "no such file" is
a verdict on either side. Any other error refuses the acquisition, or the kill
reports it.

### Claim order

The claim comes after the draining holder record is published and the
control socket is up, and before the compile-lease drain or anything else the
hold does. A kill that loses the race therefore reads a holder it can address
and stop. A cancel that wins makes the claim fail, and the acquisition ends
before any work, with its published record invalidated, its socket gone and
the flock released.

The pre-live owner invalidates whatever record it wrote. An acquisition
refused at any later step (a drain timeout, a cancel) therefore leaves no
record naming itself for a reader to take for the next acquirer's. brokkr
prints the fixed acquired line after the claim, so a waiter never says it
holds a lock it then gives up. The drain polls the shutdown flag, so a stop
accepted over the socket after the claim ends the acquisition there too.

### Kill coverage

A kill covers every waiter in the queue when the kill read it, plus the
holder it reads afterwards. A command still starting, before it joins, is
concurrent with the kill and may survive it. The holder policy stops the
holder whatever it is, including an acquisition whose cancel just committed:
a cancel guarantees the waiter does no work, not that it has released the
lock.

### Recorded scope

Scope is recorded, not inferred. The lock record, the control-socket status
and the queue record have `configured` beside `project`, since every string,
the empty one included, is a valid project key. Inferring it would let an
unconfigured command record the brokkr project's own key. A serve's registry
intent has the same flag. A record without it, from an older brokkr, reads as
configured.

## Platform tunables index

The constants stay with the subsystem whose behavior they shape. This index
points to those owners; their source definitions are authoritative, so the
values aren't copied here:

| Area | Tunable owners |
| --- | --- |
| Child output and supervision | `STATUS_WIDTH` in `src/output/console.rs`; deadline polling, output-drain settling, capture limits, the stdout-to-file runner, and forwarded-SIGTERM budget in `src/output/child.rs` |
| Lock acquisition and status service | lock polling and compile-lease drain policy in `src/lock/hold_protocol.rs`; request size, service polling, and request/reply budgets in `src/lock/service.rs`; stable lock-record read retries in `src/lock/record.rs` |
| Guard protocol | handshake probe budget in `src/guard.rs`; protocol fields shared with `src/bin/rustc_guard.rs` |
| Process cleanup | ancestry and descendant depth limits in `src/stray.rs`; orphan grace and reap policy in `src/test_orphans.rs`; child OOM adjustment in `src/oom.rs` |
| User-facing defaults | worktree retention in `src/worktree_record.rs`; short config-filter rejection in `src/config_parts/parser.rs`; `brokkr wc` default in `src/wc.rs`; man section-index threshold in `src/man.rs` |
| Check/test accounting retention | `ACCOUNTING_RUNS_KEPT` in `src/check_cmd/accounting/journal.rs`; the pruning selector accepts an explicit retention count for focused checks |

## Clippy and rustdoc phases

This section covers how the diagnostic phases report a failure their
diagnostics don't explain, and how the rustdoc phase lints without rendering.

### Unexplained failures

A failed clippy or rustdoc run whose failure the diagnostic list doesn't
explain gets its captured streams after the list (`failed_run_streams`). The
streams are narrowed by kind but never capped by line count. Two cases
qualify:

- The run left no `error`-level diagnostic at all: a build script that
  panicked, or an error an allow filtered.
- Its stderr holds an error the list doesn't have. Under `--keep-going`, one
  run can report a rustc error in one crate and a build script's panic in
  another. Hiding the panic until the rustc error is fixed is the
  one-failure-per-run loop `--keep-going` exists to break.

cargo's own summaries of a failure reported elsewhere
(`could not compile ...`, `build failed`) don't qualify.

stdout loses cargo's JSON records, since a workspace emits one
`compiler-artifact` record per unit, the bulk of the stream. The exception is
an error-level `compiler-message` the allow lists removed from the list. It
keeps its formatted diagnostic, since it's still a reason cargo failed. A
cargo line the JSON decoder rejects is already in the list verbatim, in its
parser error, including when a macro backtrace exceeds serde_json's nesting
limit.

stderr, when it holds an `error` block, prints every error block the list
doesn't already have, plus a trailer counting the warnings withheld
(`script_check`'s `rustc` rule). The rules for blocks are:

- **A failed build script's `--- stdout`** inside an error block loses its
  `cargo:` directives (one `cargo:rerun-if-env-changed=` per variable a
  `-sys` crate probes). A `(N build-script directives not shown)` line counts
  them. `cargo:warning=` and `cargo::error=` lines and the script's
  `--- stderr` stay whole.
- **Any other block** is withheld only when it is self-contained: every line
  after the header is indented, blank, or a `help:`, `note:` or `=`
  continuation. That test comes first, for listed manifest lints and cargo's
  bookkeeping lines too.
- **A block that swallowed unindented text** (relayed tool output with no
  header of its own) prints whole, since that text may be the cause.
- **The trailer** counts neither listed lints nor cargo's bookkeeping: the
  `generated N warnings` tallies, whatever their suffix, and
  `build failed, waiting for other jobs`.

With no `error` block, stderr prints verbatim. Containment is a heuristic:
foreign text that happens to be indented is still withheld with the block it
follows.

### Lint-only mode

The rustdoc phase reads rustdoc's diagnostics and never the site it renders,
and rendering (pages, search index, static files) is most of a `cargo doc`'s
time. brokkr probes once per run whether the rustdoc cargo will run lists
`--check` in its `-Z unstable-options --help`, which a nightly does
(`src/rustdoc_check.rs`). If it does, the phase adds
`-Zunstable-options --check`, which runs the same analysis and lints and
writes nothing. A toolchain without it renders as before; no key sets this
either way. The lints reported are the same: a probe with one each of the
default-on `rustdoc::` lints got identical warnings from both modes.

The flags go in cargo's rustdocflags, which cargo resolves by the same
one-source rule as rustflags. brokkr places them with the same decision
(`rustflags::place` with `FlagKind::Rustdoc`; see "Rustflags layer" for the
layer rule):

- A set `CARGO_ENCODED_RUSTDOCFLAGS` or `RUSTDOCFLAGS`, the sweep's `env`
  included, is appended to, never replaced.
- Otherwise they go in as `--config` at `target."cfg(all())".rustdocflags` or
  `build.rustdocflags`, whichever is live, merged after the project's own
  entries.

Where the layer is a guess and a target table does outrank it, the injection
is inert: the phase renders, slower but correct. `--commands` shows the
placement either way.

The same placement takes the sweep's cfg rustflags (see "Per-sweep
rustflags"), ahead of `--check`. For those, an inert injection isn't "slower
but correct": rustdoc then resolves `cfg` unlike the build, so a guessed layer
warns once per sweep.

### Doc unit freshness

Cargo judges a doc unit up to date by its output,
`target/doc/CRATE/index.html`, which `--check` never writes. On its own,
every run would therefore re-document every member, changed or not. After
each run, the phase creates a placeholder there (only if absent) for each unit
cargo ran. It sets the placeholder's mtime to the moment before cargo was
launched, so an unchanged member stays fresh and a source edited mid-run is
still newer. A fresh unit's diagnostics are replayed from cargo's cache, so a
warning keeps failing the phase on an unchanged rerun.

For a unit that ran under `--check`, a plain `cargo doc` passes different
rustdocflags, so its fingerprint differs and it renders over the placeholder.
Not every stamped unit ran under `--check`. Under a target sweep, cargo hands a
host unit (a proc-macro crate's doc unit) no rustdocflags at all, and an inert
injection reaches no unit, so those render. We accept that. A rendered unit
has its real page, so the stamp either finds it present and only rewinds its
mtime, or misses it. It misses for a host unit documented into the host doc
dir rather than `TARGET/TRIPLE/doc`, and leaves a placeholder that unit never
uses as its output. A wrong path guess (a `build.target` triple moves docs
under `TARGET/TRIPLE/doc`) leaves the unit dirty: slow, never wrong.

### Sweep tags

Sweep tags follow coverage, for clippy and rustdoc alike. brokkr tags a
diagnostic with the sweeps that reported it only when some run that selected
its package didn't. A run selects a package by `-p`, or by the default
members for a bare selection, matched through the message's `package_id`. A
shape-specific diagnostic, such as a link that breaks only with a feature off,
keeps its `[run-prep-adapter-free]`; one that every covering sweep reports is
untagged. A dependency member a run compiled without selecting doesn't count
as covered.

Rustdoc output goes through clippy's parser and formatter: one line per
diagnostic in the `error[LINT] FILE:LINE:COL MESSAGE` form, merged across
sweeps, sorted by lint with changed files first, and nothing capped.

### Rustdoc failure rule

**Any diagnostic fails the rustdoc phase, warnings included**, the rule
clippy's gate applies, for the same reason. Rustdoc's lints are
warn-by-default, so a gate that failed only on `error` would pass every broken
link unless the project also set `-D warnings`. brokkr injects no
`-D warnings`. rustdoc takes lint flags only through rustdocflags, where each
change re-fingerprints the doc units, and brokkr's own gate already fails on
warnings.

For the same reason, suppression happens at ingestion, not on argv:

- **`[lints] allow`** drops a diagnostic whose lint code equals an entry
  exactly (`rustdoc::private_intra_doc_links`). A group name matches nothing
  here, since a diagnostic has its member lint's code. Prefer `Cargo.toml`'s
  `[lints.rustdoc]` for a rustdoc lint: brokkr's list also reaches the test
  phase's rustflags, and a new entry there costs a full test rebuild.
- **`[lints] allow_exact`** drops `lint@path` sites, as for clippy. The
  stale-entry notice splits by lint namespace. This phase judges the
  `rustdoc::` entries (`rustdoc: allow_exact ... suppressed nothing`) and
  clippy judges the rest. A clippy-only site that rustdoc never reports isn't
  stale, and neither is a `rustdoc::` site that clippy never reports.

The rustdoc phase is skippable as `rustdoc` in a partial profile's
`skip_phases`, is skipped on a markdown-only tree, and has the same 5 min
ceiling as clippy.

## The test phase

This section covers how the test phase compiles and runs: lint injection, the
failure list, rustflags and direct execution, the parallel lane, the plan
`prepare` writes, and the execution accounting that audits it.

### Lint suppression

The test phase reads `[lints] allow` and `[lints] allow_exact` too, not only
clippy. It has to. A lint that a project's `-Dwarnings` turns into a compile
error fails the *build*. A suppression that cleared clippy but not the test
build would leave `brokkr check` green on one phase and red on the other for
the identical diagnostic, with no config key that reached the second.

Three things differ from the clippy phase, all forced by the fact that this
phase compiles rather than reads diagnostics:

- **`allow_exact` loses its file scope here.** The build fails during
  compilation, before any diagnostic reaches brokkr to be filtered, and `-A`
  has no path-scoped form. Each entry therefore contributes its lint name
  build-wide, and the run says so on its notice line.
- **Only rustc's own lints reach the build.** A `cargo::` entry isn't passed
  as a rustc flag, since rustc knows no `cargo` lint tool; cargo's manifest
  lints still arrive on stderr and the shared owner path reports them as
  warnings. A `clippy::` or `rustdoc::` entry isn't passed either. See
  "Rustflags layer" for why an inert flag costs rebuilds.
- **The flags go through cargo's rustflags,** since `cargo test` has no
  `-- RUSTC_FLAGS` passthrough. "Rustflags layer" states the full rule;
  [`[lints]`](../docs/brokkr.toml.md#lints-section) states why an injection
  can be deliberately inert.

The rustflags lookup covers matching target entries (including
`CARGO_TARGET_TRIPLE_RUSTFLAGS`), `build.rustflags` (including
`CARGO_BUILD_RUSTFLAGS`), and recursively included cargo config files. brokkr
composes directly into the encoded or plain rustflags environment source, or
uses `--config` at the active target or build layer. The notice line names
inherited rustflags environment sources. If a live environment value isn't
UTF-8, brokkr refuses to compose it, because the launch API can't preserve it.
If cargo config can't be fully read, the sink stays at the potentially inert
build layer, and cargo reports its own config error before compiling.

The flags reach **every call site in the run that compiles**, not only the
`cargo test` invocation:

- the sweep pre-build, which would otherwise fail on the unsuppressed lint
  before `cargo test` is ever reached
- every lane's prebuild
- the `prepare` phase's `cargo test --no-run` builds, each lane's and each
  shape's universe
- `brokkr test`'s own `build_packages` pre-build

Both pre-builds also take the sweep's pinned `feature_unification`, like
every other compiling path, so the binaries a lane's tests spawn come from the
same feature graph as the tests.

A call site missing the injection turns a run whose every lane went green
into a failure with no verdict. From the caller's side that is
indistinguishable from a real failure until you read which phase the error
came from. The universe build is where that showed up. The test that guards
it asserts the property rather than the call site: enumeration is assembled
from the same selection the allows are prepended to.

### Rustflags layer

Only lints rustc itself raises reach the test phase. A `clippy::` or
`rustdoc::` name is never raised by a rustc build, and a `cargo::` name isn't
a rustc lint at all, so their allows stay out of the build. That's
deliberate, not tidiness. rustflags are part of every unit's fingerprint, so
any flag here makes the test phase's artifacts differ from what a plain
`cargo build` or rust-analyzer builds in the same target dir. Each then
rebuilds the whole graph over the other's. An allow for a rustc lint
(`deprecated`, `dead_code`) still pays that cost; where the project can say it
in the manifest's `[lints]` instead, every build shares one fingerprint.

`cargo test` has no rustc-flags passthrough, so the test phase puts the `-A`
flags into cargo's rustflags. brokkr decides the layer per sweep, because
cargo does **not** merge rustflags across kinds of source. It picks exactly
one, and the first match wins:

1. `CARGO_ENCODED_RUSTFLAGS`
2. `RUSTFLAGS`
3. every *matching* `target.TRIPLE.rustflags` or `target.CFG.rustflags`,
   joined together
4. `build.rustflags`

That's why brokkr doesn't export `RUSTFLAGS`. Doing so promotes its own flags
to source 2 and discards the project's `-Dwarnings`, its `-fuse-ld=lld` and
its `relocation-model=pic`, all at once. It's also why `[[check]] rustflags`
isolates a sweep into `target/rustflags-HASH` and shouldn't be used for a
lint allow.

Instead, brokkr finds the layer already live and adds to *that* one, where
cargo's own array merging applies and brokkr's entry comes last. A lint allow
needs that, since rustc resolves conflicting lint levels last-wins. brokkr
reaches a config layer with `--config`, the highest-precedence config source,
so the project's `.cargo/config.toml` is untouched.

Cargo refuses to merge an array with a string, even from `--config`. When a
config file sets the chosen key as a string (`build.rustflags = "-Dwarnings"`),
brokkr refuses the placement, naming the file, rather than emitting an
invocation cargo would fail. Writing a string instead would reorder the
project's flags against env contributions to the key. A string and an array
for one key across files, or for `build.target`, is a merge cargo refuses
too, and leaves the decision undecided on the inert side.

brokkr inspects the whole chain cargo reads. That's every
`.cargo/config.toml` from the build root's physical path upwards (cargo's
`current_dir()`, so a symlinked build root walks its target's ancestors), plus
the cargo home's, and every file they include. Includes are walked as cargo's
loader walks them: an include is the path it was reached through, so two
symlinked spellings of one file are two documents with their own relative
children. A user-level `[target.HOST_TRIPLE] rustflags` is common (a linker
choice, a `-Ctarget-cpu`). It silently decides the winning layer for **every**
project on that machine, including ones whose own flags live in
`build.rustflags` and are therefore already ignored.

An unrecognised `cfg(...)` selector resolves to the inert direction, never the
destructive one. Injecting at `build.rustflags` when a target table actually
wins does nothing. Injecting at `target."cfg(all())"` when `build.rustflags`
was the live layer would demote it and drop the project's flags.

The target layer is judged per triple the invocation builds for: a sweep's
`target`, else `build.target` or `CARGO_BUILD_TARGET`, else the host. A
`cfg(...)` table is judged with the host's values only for the host triple.
For an ambient cross triple, only a selector needing no platform fact
(`cfg(all())`) decides anything. One `--config` key serves the whole
invocation, so brokkr chooses `target."cfg(all())"` only when the target layer
is live for every triple; triples that disagree get `build.rustflags`, on the
inert side. A set-but-empty `CARGO_TARGET_TRIPLE_RUSTFLAGS` doesn't make the
target layer live: cargo falls through to `build.rustflags` when the joined
target source is empty.

The run always prints which layer it used, so an inert injection is
diagnosable rather than mysterious:

```text
test: allowing deprecated via --config target."cfg(all())".rustflags ([lints] allow)
```

### The failure list

A sweep's `cargo test` invocation runs several test binaries: the lib unit
tests, then one per `tests/*.rs` integration file, then doctests. Cargo
concatenates their output into one stream. Each binary prints its own pair of
`failures:` sections, the captured detail blocks and then the bare name list.
The parser resets its section state at every `running N tests` line, so the
rendered `cargo test: N failures` list names failures from **every** binary,
not just the first one that failed.

That reset is load-bearing, and a regression test guards it
(`failures_in_every_suite_are_reported_not_just_the_first`). Without it, the
state latches on the first suite and a failing lib test masks every
integration failure behind it. The exit code stays correct, so the run is
honestly red; only the which-tests list is short. That's the worse of the two
shapes, because a fixer who clears every listed failure concludes the run is
understood.

The reset alone isn't enough. Three more rules close the remaining holes, each
in its own section below: "No fail fast", "Failure roster" and "Failure
count". A short list once led two agents in consecutive rounds to
mis-attribute which test covered a mutation.

A failing sweep ends the run before the later `[[check]]` sweeps test at all,
so their results are absent from a red run. That follows from phase ordering,
not from any reporting limit.

### No fail fast

**The sweep passes `--no-fail-fast`.** Without it, cargo stops after the first
test *binary* that fails, so the later binaries never run and no renderer can
report their failures. `brokkr check` is a whole-tree gate, so it enumerates
every failure it can reach in one run. A caller that passes its own
`--no-fail-fast` isn't double-flagged.

`--no-fail-fast` needs **harness isolation** to be safe, and the serial lane
provides it. In one shared libtest stream, a harness that dies mid-test (an
abort or a stack overflow, which is what a mutation produces) leaves its suite
open. The next harness's `suite/started` then arrives inside it and must be
ignored, since a live test can print that record. Meanwhile the per-test
clock keeps billing the dead test until it kills the next, healthy harness as
its hang. Nothing in the shared stream can tell a dead suite from a forged
boundary, so brokkr stops sharing it.

### Harness shim

brokkr installs itself as cargo's host target runner and as rustdoc
(`src/test_runner/harness_shim.rs`). Each harness cargo launches is brokkr
first. It hands brokkr the read end of a fresh pipe over a unix socket, puts
the write end on its stdout, and execs the real harness, keeping its PID and
parent.

brokkr authenticates each connection from the kernel, once per process
identity. `SO_PEERCRED`'s PID must have the root cargo as its parent, and its
executable must be brokkr. brokkr drains every pipe into its own
reconstructor and tracker. A pidfd decides liveness: a dead harness stops
being billed even if a leaked descendant holds its pipe open, while a live
test that only closed stdout is still billed. A run-level idle clock covers
the gaps between harnesses, including rustdoc compiling doctests after the
last one, so a gap isn't charged to the previous suite.

A harness that died with its suite open has that suite closed for it: its
real failures rendered, and the in-flight test printed as a **suspect** only.
Cargo's own signal line becomes a `CRASHED HARNESS (signal: ...)` entry in the
failure list. That closure is display only. It never reaches the execution
accounting, which reads the records the harness actually wrote.

A connection that isn't cargo's direct child (the doctest runtool under
rustdoc, anything nested) runs unisolated. The shim scrubs its own environment
from the program it execs, so a test that runs cargo itself gets the
toolchain's own.

### Shim handshake

Before the shim gets its pipe, it says what it is in a one-message handshake.
The handshake names its role, `harness` or `rustdoc`, and for a harness the
executable cargo asked the runner to run. Cargo runs
`RUNNER EXECUTABLE ARGS`, so the runner argv is the one place cargo states
which test binary a harness process is, and the stream is attributed to that
executable.

Under a `certifies = "complete"` profile the session is **strict**:

- The executable must resolve against this lane and resolution's planned
  artifacts; a path planned for another lane doesn't satisfy it.
- A cargo child that fails the handshake or names an unplanned executable is
  refused, and the refusal recorded as an attribution error.
- The shim **fails closed**: it exits instead of exec'ing a harness on the
  shared stream.
- After a run cargo reports green, a planned harness with expected executions
  that never presented its stream is an attribution error too.

Outside a complete profile the session stays permissive: a harness that can't
be isolated runs on the shared stream with a warning. On a lane that verifies
its own build (see "Serial lane entry"), every connection, in either mode,
first waits for that verdict; only a strict session refuses on it.

### Rustdoc handshake

A rustdoc handshakes only when it runs doctests: `--test` in its argv, or
inside an `@path` argument file (cargo moves a long rustdoc command line into
one). An argument file the shim can't read counts as running doctests, so it
can never slip past a strict session. The handshake names the package
directory cargo runs rustdoc for (`CARGO_MANIFEST_DIR` in cargo's environment
for the doctest invocation).

A **derived doctest carrier**'s strict session holds a table of every member
library rustdoc can test, by canonical manifest directory, built from
`cargo metadata`. After the usual authentication, it admits rustdoc per
library:

- A library whose manifest leaves `doctest` on gets its pipe.
- A library whose manifest sets `doctest = false` is **declined**. The decline
  is journaled first, then the shim exits 0 without running rustdoc, since an
  explicit `cargo test --doc` would test it regardless.
- A directory the table doesn't hold, or none at all, is refused as an
  attribution error. Only a positively identified library may be either run
  or declined.

A configured `doc_only` lane holds no table: its author asked for `--doc`, and
gets cargo's meaning of it.

### When the shim can't be installed

Some sweeps can't take the runner without overriding something:

- a runner already configured for the host
- a configured rustdoc
- an ambient target other than the host (`CARGO_BUILD_TARGET`,
  `build.target`) on a sweep without its own `target`
- a forwarded `--config` or `--target`

Such a sweep drops `--no-fail-fast` instead, warns why, and stops at the
first failing harness: an honest short list rather than a wrong name. A
complete profile refuses such a lane during `prepare`, with the reason. Every
harness would share one stream, and an unattributable stream can't be
accounted.

The runner names brokkr as `/proc/PID/exe`, not its install path. Cargo
splits a runner string on whitespace. A binary replaced mid-run (a
`brokkr install` elsewhere) reads back as `PATH (deleted)`, which has a space
and no longer exists. The magic link has no whitespace and still execs the
replaced inode, and the shim authenticates its peer by inode rather than by
link text. The threat model is accidents and misbehaving tests; project code
that deliberately attacks this IPC is out of scope.

When the cargo argv names a `--target` other than the host (a
`[[check]] target` sweep), brokkr installs the shim under that triple's runner
variable as well as the host's. Cargo runs the target's test binaries through
one and host units (a proc-macro's test harness) through the other.

### Failure roster

**The `failures:` name list is the authoritative roster.** The captured detail
blocks are best-effort: an aborted suite can truncate the stream, and a detail
block can hide inside another test's captured output. Any name the roster
lists that the detail pass missed is appended bare (name, no location) rather
than dropped.

A suite that never summarised, because its harness crashed after failing some
tests, printed no name list at all. For that one unclosed suite, its
`test NAME ... FAILED` verdicts stand in for one. Only for it: a
verdict-shaped line a passing test prints into a suite that finished adds
nothing.

### Failure count

**libtest's `test result:` tally outranks the parsed roster for the headline
count.** `cargo test: N failures` reports `max(tally, roster.len())`. When
the tally is larger, the report says how many failures couldn't be attributed
to a test name. An empty roster doesn't route to the compact "all passed"
summary when the tally is non-zero.

`the_name_list_completes_a_short_failure_roster`,
`the_rendered_report_names_every_failing_test` and
`an_unattributable_failure_is_counted_not_dropped` pin these rules.

### Foreign manifest warnings

A passing test sweep and its support pre-builds parse compiler messages from
cargo JSON, and manifest lints from stderr, into `Diagnostic` values.
`OwnerIndex` classifies both package ids and manifest paths, so a warning from
a path dependency outside the workspace is dropped by the same rule clippy
uses. Workspace member and root-manifest warnings still print. brokkr renders
the remaining warnings and holds them, once per distinct diagnostic, until
the phase ends.

### Per-sweep rustflags

A `[[check]]` entry may set `rustflags`, a token list such as
`["--cfg", "madsim"]`. brokkr exports it as `RUSTFLAGS` on that sweep's cargo
processes only: clippy, rustdoc, the test-phase pre-build and the test run. It
is **composed** with any inherited `RUSTFLAGS` by appending. brokkr uses
`CARGO_ENCODED_RUSTFLAGS` instead when the environment already has the encoded
form, which cargo would otherwise let shadow `RUSTFLAGS`, or when an argument
holds a space the plain form's split would break. brokkr sets no `RUSTFLAGS`
of its own (`--cap-lints=warn` is a cargo `--` arg, not a rustflag), so this
composes only with what the caller's environment has.

Cargo resolves rustdocflags by the same rule but never hands rustdoc
`RUSTFLAGS`. Its doctest arguments are externs, search paths, features,
check-cfg, build-script cfgs and the unit's rustdocflags. Without more, a cfg
sweep would be documented with the cfg off, and its doctests collected and
compiled with it off against a library built with it on.

So brokkr places the `--cfg` and `--check-cfg` entries in rustdocflags too
(`build_purpose::environment::place_sweep_rustdocflags`).
`rustflags::cfg_args` extracts them, walking options and operands so another
option's operand is never read as one. They go:

- in the rustdoc phase, beside `--check`
- in `lane_env`, and `brokkr test`'s `sweep_env`, for every cargo invocation
  of a lane

Uniform placement is safe there because cargo hashes rustdocflags into doc
units only, so a pre-build or a lane running no doctests compiles exactly as
before. An env placement only ever appends to a source that was already set.
A test that launches cargo itself therefore inherits the cfg on a
`RUSTDOCFLAGS` it already inherited; a `--config` placement doesn't reach test
processes. The composed pair replaces its key in `LaneEnv::env`, which every
cargo run of the lane takes as is, so a sweep's own `RUSTDOCFLAGS` can't undo
it.

A global cfg such as `--cfg madsim` reshapes every fingerprint in the build
graph. A sweep with non-empty `rustflags` therefore **auto-isolates** into its
own target dir, `target/rustflags-HASH`, the hash keyed on the flag content.
Sharing the default `target/` would force a full recompile in both directions
every time the sweep alternated with the plain sweeps. Isolation keeps the two
caches apart, and brokkr recomputes `BROKKR_TEST_BIN_DIR` and
`CARGO_TARGET_DIR` against the isolated dir. Sweeps with *identical* flags
share one dir, so several madsim legs compile the simulator once. Isolation
is automatic, and there is no `target_dir` key. Setting `RUSTFLAGS` or
`CARGO_TARGET_DIR` in the entry's `env` alongside `rustflags` is a parse
error.

An entry may also set its own `tests`, `skip` and `only` libtest filters.
They are **ANDed** with a referencing profile's filters of the same name: they
append, never replace. This expresses a curated per-package subset as several
sibling `[[check]]` entries under one profile. A madsim gate needs that shape:
one crate runs a single named test, another runs a `virtual_time`-filtered
set, all under the same shared isolated target dir. The `sim` worked example
in `docs/brokkr.toml.sweeps.md` shows it.

### Direct execution

Cargo does more than exec a test binary, so brokkr reproduces its launch
contract per binary (`src/check_cmd/direct_runtime.rs`, read against cargo's
`Compilation::fill_env`). The envelope is:

- **cwd**: the owning package's root.
- **Dynamic-loader path**, in cargo's order: the build-script link-search dirs
  inside the output dir, sorted; the output dir; the deps dirs; the toolchain
  libdir; then the inherited value. The output dir is `target/PROFILE`, found
  from where the executable sits, in either the `deps/` or the newer
  `build/PKG/HASH/` layout. The inherited value is used alone when it already
  starts with cargo's entries. A link-search dir outside the output tree is
  left out, as cargo leaves it out, because it's likely to shadow a system
  library.
- **`[env]`** from the cargo config chain with `force` and `relative`
  semantics, merged per key across the chain as cargo merges it. A higher
  table setting only `force` keeps a lower `value`, and `relative` anchors at
  the file that supplied `value`.
- **Package variables**: `CARGO_PKG_*`, `CARGO_MANIFEST_DIR` and
  `CARGO_MANIFEST_PATH` from cargo metadata. Absent manifest fields export as
  empty, matching cargo.
- **Build-script `rustc-env` values**, from the prebuild's
  `build-script-executed` messages.
- **`CARGO_BIN_EXE_NAME`** at runtime, for the package's own bins.
- **`CARGO`**: the cargo executable itself, resolved through rustup when
  rustup is the `cargo` on PATH, since cargo exports its own path, not the
  proxy's.

The envelope does **not** include `OUT_DIR`. Cargo exports it to build
scripts and rustdoc only, never to a test process it launches, so a runtime
read of it fails under cargo and must fail here too. `CARGO_TARGET_TMPDIR` is
deliberately not reproduced at runtime either: cargo's contract makes it
compile-time (`env!`) only. Cargo-owned values are applied last, so a sweep's
`env` can't forge them.

A package whose build script ran more than once in the build with different
`rustc-env` output refuses the envelope: built for two targets, say, or under
two feature sets. The stream doesn't say which run the executable linked
against.

Two refusals keep the direct launch honest. Each refuses every
direct-execution path (this lane, the process-isolated one, and `brokkr test`)
at resolution time:

- a configured target runner, `[target.TRIPLE].runner` or
  `CARGO_TARGET_TRIPLE_RUNNER`: a qemu, wine or valgrind wrapper that direct
  execution would silently bypass
- any ambient build target, `build.target` or `CARGO_BUILD_TARGET`, the
  host's included, since setting one moves the artifacts

brokkr reads both from the environment cargo builds under, so a sweep `env`
setting either counts. A sweep's own `target` key isn't an ambient build
target (see "Target sweeps").

An unreadable or malformed selected cargo config, or a missing required
include, also refuses launch reconstruction. brokkr reads the chain once per
lane, when its environment is decided. That one snapshot feeds the runner
refusal, the serial shim decision and the direct runtime, so such a config
fails every lane kind, serial included, before it builds. Missing config
search locations and missing optional includes are accepted as absent.

Forwarded cargo args that direct execution can't honour are rejected:
`--no-run`, `--target`, `--config`, `--manifest-path` and `--target-dir`.
Other forwarded args are handled as follows:

- Other cargo args shape the direct lanes' prebuild, and target selectors
  narrow the prepared plan.
- Libtest args shape listing and selection, and the parallel lane replays
  them for each binary.
- The process-isolated lane keeps `--exact CASE` and `--test-threads=1` on
  each child. It applies positional filters during planning and forwards
  option args to each one-test child.
- `--format` and `--test-threads` stay runner-owned.

### Target sweeps

A `[[check]]` entry's `target` is a `TargetSpec` (`src/target_platform.rs`)
in the declaration: a literal triple or a `{host_arch}` template. Config load
validates it for spelling only, since what a template names depends on the
host.

Planning turns it into the concrete triple the `PlannedSweep` holds
(`plan_sweep`, `src/check_cmd/selection.rs`). `plan_sweep` is the one
production path from a declaration to a planned sweep, used by
`resolve_invocation_sweeps` for `check`, by its lint lanes after cadence, by
`brokkr clippy`, and by `brokkr test`'s sweep decision. A template takes the
architecture of the host triple reported by the compiler cargo would run under
the sweep's composed env, read through `cargo::rustc::load_selected`. A
literal is itself and probes nothing. Every consumer reads the concrete triple
through `PlannedSweep::target`. The spelling stays on the declaration
(`SweepDecl::target_spec`, still reachable through the planned sweep's
deref), which nothing past planning reads.

The triple is build shape: `EffectiveBuild` and `DeclaredShape` encode it.
The shape id encoding appends it only when set, so every host shape id is
unchanged, and a template and a literal naming the same triple share an id.
`build_purpose::sweep_args` puts `--target TRIPLE` on every cargo run of the
sweep.

Each test lane resolves the target once, when its environment is decided
(`resolve_sweep_target` in `src/check_cmd/prepare.rs`, over
`SweepTarget::resolve` in `src/target_platform.rs`). brokkr finds the
compiler cargo would run by cargo's precedence (`RUSTC`, `CARGO_BUILD_RUSTC`,
`build.rustc`, `rustc`) under the sweep's env. Through that compiler:

- the sysroot must hold the triple's standard library
- `rustc --print cfg --target` gives the triple's cfg facts
- `--print target-libdir --target` gives its libdir

`cargo --version` must be 1.89 or newer, since an older cargo builds a cross
target's doctests and doesn't run them. The cfg facts decide only names the
triple alone fixes. Every other name is undecided, and an undecided atom
makes its whole selector undecided.

The two halves are two types:

- **`TargetFacts`** is the triple and its cfg facts, read by
  `TargetFacts::query`, refusing nothing. Everything matching cargo's
  `[target.*]` tables takes it: `src/rustflags.rs` and
  `place_sweep_rustdocflags`.
- **`SweepTarget`** is the preflighted lane target, made only by
  `SweepTarget::resolve`. It holds `TargetFacts` plus the libdir, and the
  libdir consumers take it.

The rustdoc phase reads facts alone (`sweep_target_facts`), for tested and
lint lanes alike. It runs no doctest and links no binary, so the preflight's
refusals protect nothing there, and a lint lane keeps the no-preflight rule
its docs state. `place_sweep_rustdocflags` takes the facts as a supplier it
calls once when it has flags to place and never otherwise, so a rustdoc run
with nothing to place queries no target facts.

The facts feed three consumers:

- **Runner discovery** (`CargoLaunchConfig::discover_for`) adds the triple's
  runner: its table, its env spelling, or a `cfg(...)` table not proven false
  for it. The lane refuses it in every lane kind. brokkr executes the sweep's
  binaries itself (the serial lane lists them too), so a runner would be
  bypassed, and emulation through a runner is unsupported. Kernel
  `binfmt_misc` emulation is invisible here and runs like native execution.
  The same discovery drops an ambient `build.target`, which the explicit
  `--target` overrides.
- **The rustflags sink** matches the triple's `target.*` tables against the
  facts.
- **The direct runtime** picks each binary's toolchain libdir by its exact
  output root. `BASE/TRIPLE/PROFILE` is the target's and `BASE/PROFILE` the
  host's, for `BASE` each of the target dir and build dir cargo metadata
  reports under the lane's env. A root matching neither, or both with
  different libdirs, refuses the envelope.

Forwarded `--target`, `--config`, `--manifest-path` and `--target-dir` are
refused on a target sweep in every lane, since each moves something the
resolution fixed.

### Target host differences

brokkr doesn't check before building whether the machine can execute a
target lane's binaries, and nothing refuses a triple on that ground. A probe
binary built ahead of the lane proves nothing about the lane's own files. A
`binfmt_misc` registration can match header bytes such as the ELF type, and
the ELF loader rejects files for reasons beyond the architecture. A refusal on
the triple's cfg facts would refuse emulated hosts that run such lanes.
Executing the lane's binaries decides, and the spawn error names the file and
the OS error.

What brokkr says instead is a fact. `announce_target_hosts`
(`src/check_cmd/prepare.rs`) compares `rustc --print cfg` with and without
`--target` from the selected compiler, on `target_os`, `target_arch`,
`target_pointer_width` and `target_endian` (`target_platform::differing_host`).
For each lane the test selection attempts whose facts differ, it narrates one
line naming the sweep, the triple and the compiler host.

`plan_check` says it after selection, so the audit shows it, and a disabled
test phase, an excluded lane or a lint lane says nothing. `brokkr test` says
it once its selection is settled. A failed query leaves the line out; it
never fails planning.

### Prebuild feature unification

Direct execution leaves feature unification unchanged on the prebuild side.
The prebuild and enumeration add
`-Zfeature-unification --config resolver.feature-unification="workspace"` on
a sweep whose selection is exactly the whole workspace. That means all of:

- no `packages` and no `test_exclude_packages`
- no CLI `-p`
- no package selector among the args forwarded after `--`, in every spelling
  cargo accepts, the attached `-pfoo` short form included
- `default-members` isn't a subset

That promotion is what the default `feature_unification = "auto"` means, and
it keeps deciding which graph the binaries are *built* from, so existing
configs compile what they compiled before. Direct execution removes any
runner-side need for the pin, so brokkr honours an explicit `selected` on a
promotion-eligible sweep (see
[Feature unification](../docs/commands/check.test-lanes.md#feature-unification)).

A cargo too old for the flag fails the prebuild with a named remedy; there is
deliberately no silent fallback. Consumer workspaces can get the one-graph
property for every cargo invocation, rust-analyzer and ad hoc `-p` builds
included, by committing the equivalent `[unstable]` and `[resolver]` pin in
`.cargo/config.toml`.

### The parallel lane

The parallel lane runs a sweep's test binaries at once under a budget of
in-flight tests. Each binary claims a slice of the budget proportional to its
serial cost from the previous run, the sum of its own tests' durations. The
claim is:

- floored at one slot
- capped at the binary's own test count
- capped again where extra threads stop helping (`serial / slowest_test`)

Costs live in `.brokkr/parallel-timings.toml`. A tree with no history weights
by test count instead, so a budget change takes two runs to measure, not one.
The pure allocator combines these rules and orders binaries largest-first.
Serial cost is stored as integer milliseconds, not wall time, because wall
time depends on the slots granted and feeding it back oscillates (see
"Parallel allocator rationale").

The budget resolves at profile resolution, where `LaneExec::resolve` keys
only on the presence of `parallel`, so loading never depends on the host. The
budget forms are:

- **Unset** (`parallel = {}`): the physical cores in one last-level cache
  domain that the calling thread may use. `brokkr env`'s `l3 domain:` line
  prints the topology count.
- **`budget = "cores"`**: every allowed physical core, counted as distinct SMT
  sibling groups restricted to the allowed set. It fails closed to
  `available_parallelism()` when any allowed CPU's sibling list is unreadable
  or two lists overlap without matching.

Visible cgroup v2 quotas cap both. `LaneBudget` passes the policy and probe to
the plan line, which prints it after the number
(`budget 12 (cores) in flight`); an explicit count prints bare.

The lane writes two lines to the run log, however many binaries run:

```text
[run]     test fanout: 35 binaries, budget 24 in flight (claims 1-3)
[run]     test fanout: 4210 passed in 31.2s (35 binaries, built in 48.9s,
          slowest broadarrow-worker/ba-worker 28.7s)
```

On stdout its tests fold into the grouped test line, which names the slowest
parallel binary across every sweep, with the sweep it came from.

The `claims N-M` spread is the diagnostic for whether the sweep actually
overlapped. Two binaries that each claim the full budget can't overlap. That
is the failure the proportional claim rule exists to prevent, and it otherwise
reports success, so that case alone prints a warning naming the binaries
(`N binaries each claimed the full budget (B) and could not overlap`). The
warning is worded as what the claims prove: smaller binaries may still overlap
around them, so it doesn't claim the lane ran serially. A budget of 1
serializes by design and never warns.

A passing binary prints nothing of its own. Thirty-five green lines say only
what the summary says, and a reader who learns to scroll past the normal case
scrolls past the abnormal one too. To watch one run, run it:
`brokkr test NAME` streams a single test's output live.

Build time and fan-out time are logged separately, because a lane judged on
wall time must not fold the compile into that number. **The slowest binary is
named because it is the sweep's floor**: the sweep finishes when it does, so
it is the one to split, or to move to the serial lane. The grouped line has
the slowest across sweeps; each sweep's own floor is in the run log, which is
where a tuning session reads.

Failures are the exception. A failing binary prints its label, its
copy-pasteable cargo line and its output. brokkr buffers every binary's output
and reports it after the join, in plan order, so concurrent binaries can't
braid their failures together and two identical red runs produce identical
output.

The parallel lane is mutually exclusive with `isolation = "process"`, refused
before any test runs. Doctests can't run on a parallel sweep, so
`[test] doctests = true` requires at least one entry without `parallel` (a
load error). A `certifies = "complete"` profile runs a parallel sweep's
doctests in the carrier it derives for the sweep's build (see "Derived doctest
carriers").

Cargo target selectors passed after `--` (`brokkr check -- --test read_paths`)
shape which binaries the sweep plans, rather than being appended to each
per-binary command. Cargo unions selection flags, so copying a selector onto
every invocation would run that target once per planned binary. Every
selector kind is honoured with cargo's meaning:

- `--test NAME` or `--test=NAME`, `--bin`, `--example`, `--bench` (names may
  be globs)
- `--lib`, `--bins`, `--examples`, `--benches`, `--tests`, `--all-targets`

Selectors union with the sweep's own `tests` filters: `-- --lib` on a sweep
with `tests = ["cli"]` plans the lib harness and `cli`. The full semantics,
and the rule for partitioning a suite across a parallel and a serial entry,
are in `docs/brokkr.toml.sweeps.md`.

### Identities

The plan and the accounting key on three identities:

- **Binary unit**: (package id, normalized target kind, target name). The kind
  is normalized the way cargo's selectors read it, `lib` for every library
  flavour (`rlib`, `cdylib`, `proc-macro`, ...). The executable path is launch
  metadata, not identity. nextest's binary ids map onto units through the
  lane's artifact index. Reports name a unit by its nextest-style id (`pkg`,
  `pkg::target`, `pkg::bin/name`).
- **Pair**: (build shape, cargo resolution, binary unit, test). There's no
  conditional keying: two binaries of one package defining the same test path
  are two pairs on every lane.
- **Expected execution**: (lane, pair, attempt). The same pair selected by two
  lanes is two executions, each accounted on its own. No lane retries, so the
  attempt is always 1. The harness invocation isn't part of the key but is
  enforced: within a lane a pair must run in exactly one harness process, and
  an execution belongs to the first stream that reports it. A start or
  terminal for it on any other stream is a `duplicate_execution` anomaly, and
  that stream's result (pass or fail) is never taken.

### The plan

The `prepare` phase prepares every lane the test selection attempts before
any test runs. A lane the selection excludes is recorded as skipped and builds
nothing (see "Lanes that are not eligible"). For each prepared lane:

1. Its declared support builds (`build_packages`) run first, so a listing
   whose static constructor or custom harness reads a support binary sees the
   one the lane runs with.
2. Its shape is built with the same `cargo test --no-run` the lane itself
   uses.
3. Every binary is listed under the launch envelope execution will use
   (`DirectRuntime::envelope`: cwd, loader path, `[env]`, `CARGO_PKG_*`,
   build-script env), since a listing is the binary's own code and a custom
   harness may read its environment.

Under a complete claim, each shape resolution's universe is enumerated too:
`cargo test --no-run --tests` plus every explicit target selector (`--lib`,
`--test NAME`) any lane of the shape declares, projected onto a package-mode
resolution's one package. `--tests` alone leaves out a target whose manifest
sets `test = false`, which an explicit selector still builds. Without the
union, a lane running such a target would hold a binary its shape's universe
lacks. A `test = false` target no lane names stays out.

### Plan contents

The plan records:

- per shape resolution under a complete claim, its universe and its
  `#[ignore]`d subset
- per prepared lane, its selected pairs and the executions they imply.
  Ignored names are excluded unless the lane lifts `#[ignore]`; filtered-out
  names and `#[bench]` functions are outside the claim.
- the dead-filter findings (see "Preparation passes")
- a content hash (xxh3) of every test executable
- the fingerprint of the runtime index the launch envelope is built from:
  build-script env, out dirs and link-search dirs, and every support bin
  behind `CARGO_BIN_EXE_NAME` by path and content hash. Every lane kind that
  executes binaries has it, the nextest engine lane included.
- the executables each `build_packages` pre-build produced, by package,
  target, path and content hash, hashed after the lane's own build. A support
  package outside the test selection reaches the tests through
  `BROKKR_TEST_BIN_DIR`, which no test build's artifact stream names.
- an explicit marker for anything that couldn't be prepared

A lane's ignored-only listing is its filter argv with `--ignored` in place of
any `--include-ignored`, since libtest refuses the two together.

### Preparation passes

Preparation has a build stage (`build_profile`), followed by lane inventory
and policy passes in `prepare_profile` (`src/check_cmd/prepare.rs`). A listing
is binary execution under the same 20s wall limit as a test, and must never
share the machine with a compile. The passes are:

1. **Build (warm-up).** Every attempted lane's builds run, grouped by target
   directory as the clippy phase groups them: one chain per directory in
   configured order, the chains concurrent, so the compile work runs in
   parallel. Under a complete claim a lane's universe builds first, then its
   `build_packages` support builds, then its shape. Nothing is hashed, listed
   or executed in this step. A lane whose build fails here is reported in step
   2 and not built again. The `background` script checks are joined at the end
   of this step.
2. **Inventory.** Lane by lane in configured order, with nothing else running.
   See "The lane snapshot" for what this step reads and checks. Each lane ends
   this pass with one inventory disposition: `prepared` (including any
   unlistable binaries), `skipped`, `unavailable`, or `doc_tests` for a
   doctest-only lane. A skipped lane has no selected executions, and an
   unavailable lane has no binary inventory.
3. **Policy (complete claims only).** After every lane's inventory
   disposition is fixed, each shape resolution's universe is built again,
   alone, and snapshotted straight after that build, as a lane's is. brokkr
   then enumerates the universe and its `#[ignore]`d subset, validates that
   the prepared binaries belong to that universe, and judges filter liveness
   across the lanes. A skipped or unavailable inventory makes the relevant
   filters unknown, not dead. This pass writes the shape and dead-filter
   records; it doesn't alter the selected pairs. Liveness is judged binary by
   binary (`FilterLedger::judge_lane`), each binary against its own names and
   `#[ignore]`d subset. Every filter of a listed lane is registered before any
   binary is, so a lane holding no binary still reports its filters.
4. **Lane-evidence liveness (every other run).** No universe exists, so pass
   one took a second, unfiltered pair of listings per binary
   (`existence_listing`) on the libtest lanes whose sweep has a filter and that
   forward no libtest args (`wants_existence`). These listings are auxiliary to
   the inventory: a failure leaves the binary without evidence and never fails
   the lane. The same ledger then judges the lanes that gave sound evidence,
   and treats every other lane as unknown (`advisory_dead_filters`). Unknown
   lanes are the unattempted, unavailable, nextest and doc-only lanes, a
   serial lane with doctests, a lane forwarding libtest args, and any binary
   without evidence. Dead filters are kept on `ProfilePrep`, not the plan,
   whose policy universe stays empty. Once the test phase ends they are
   reported, and fail a green phase as `coverage` findings.

A lane that narrows nothing by name (no filter, skip or forwarded libtest arg)
and names no target explicitly also names, in the run log only and once per
executable, every harness it listed with no tests at all. Such a harness is
built and spawned on every run for nothing, which `test = false` on its
target would spare.

### The lane snapshot

In the inventory pass, each lane is built again, alone, with its support
builds. That's usually a no-op on what the warm-up left. It re-uplifts or
relinks whatever a later build in its directory replaced in place, and
recompiles where a build script reruns on every build.

Straight after that build, brokkr hashes the executables the lane runs or
hands its tests: test executables, the runtime `CARGO_BIN_EXE_*` bins and the
support executables. That's the lane's **snapshot**. It's both the record of
the lane's own build and the observation just before its listings, since
brokkr runs nothing between that build and the read. A dylib a build script
placed in a link-search directory isn't in it, as no artifact stream names one
(the same limit the rebuild comparison has). Every fact of the plan, support
executables included, comes from the one build it is listed from.

The snapshot is checked again after the listings. A file that changed while
the lane's own binaries were being listed fails the lane, since its inventory
would describe other binaries than the ones it runs. Hashing only after the
listings, or only at lane entry, wouldn't do. A file an earlier lane's listing
replaced, and this lane's listing restored, would pass both while the
inventory came from other bytes.

The authoritative builds run one after another, so a later lane's build can
still replace an earlier accepted lane's file: a build script writing outside
its own outputs, or a shared uplift path. The policy pass's universe builds can
too. The lane-entry check finds that, and a lane whose build isn't
reproducible then fails a complete claim.

Listings must not modify the files they are listed from. Hashing either side
of a listing catches a net change, not a file replaced and put back while it
ran.

### Content hashing

Every check is a full content read. No digest is ever served from a file stamp
(inode, size, mtime, ctime). A settled stamp says an ordinary write would have
been noticed, not that the bytes are the same: a store through a live writable
shared mapping changes them with no timestamp update.

What brokkr doesn't repeat is a read within one observation. A lane listed
from its snapshot records the snapshot's digests as its inventory's executable
identities, in its runtime-index fingerprint and in its support fingerprint,
nextest lanes included. That's sound because the snapshot was read just before
the listings and is checked again after them, before the inventory is
accepted. A path the inventory names that the snapshot doesn't hold is
refused. A serial lane's merged and per-resolution fingerprints share one
digest per bin. A lane prepared at lane entry has no snapshot and reads its
files.

The reads of one snapshot or recheck are spread over a process-wide budget of
a few readers and joined before the caller goes on, so a listing never starts
before its snapshot is complete. Drift lines stay in path order, and a stop
during the reads is a stop, never drift. The run log has one line per hash
site for the prepare phase and for the test phase: files, bytes read, and read
time summed over readers.

### The plan record

brokkr writes the plan to `.brokkr/accounting/RUN_ID/plan.json`, under the
config dir, before the first test executes (see "Retention"). A journal
beside it, `journal.jsonl`, receives every observation as it happens, one JSON
line each, written straight through. The journal survives anything short of
the machine going down, the watchdog's backstop exit included. The journal's
closing line is written when the test phase ends; a journal without it was cut
short, and the audit says so.

### Unpreparable lanes

A lane that can't be prepared, through a build failure or a refusal, is
recorded as unavailable, and preparation goes on with the rest, so every
refusal is reported in one run. A stop (an interrupt or the phase ceiling)
ends preparation at once.

Under a complete claim, either condition leaves an incomplete plan
(`plan_complete = false`), and the test phase doesn't run: a record taken
against an incomplete plan could certify nothing. `failed_phase` is then
`"prepare"`. The audit then says the test phase didn't run and how many
executions were held back. It doesn't reconcile an empty journal into
thousands of `unobserved` executions and unfinished doctest carriers.

Under a profile that doesn't certify, a lane that can't be prepared is
recorded as unavailable and still runs through its normal lane path. The run
can report its outcome without claiming a whole inventory (see "Inventory and
certification").

### Direct lane entry

Before a lane that executes its binaries directly runs (parallel, isolated,
the nextest engine), brokkr hashes its snapshot again, with no cargo
invocation. If every file in it still holds the planned content, the lane runs
its prepared snapshot: the planned binaries under the envelope they were
listed with.

Rebuilding here would only ask whether building again changes anything. A
project whose build scripts rerun on every build answers yes even when nothing
changed: drift the lane never had, and on a complete claim a hard error. What
this check claims is that the lane's snapshotted executables matched the plan
immediately before it ran. Outside it are a test that replaces a support
binary while the lane runs, a detached process a build script left behind,
and a dylib in a link-search directory.

### Serial lane entry

A serial or doc-only lane runs `cargo test`, which builds before it runs
anything, so what it verifies is the build that executes. No build of its own
happens before it, and the lane enters cargo once. Its `cargo test` already
streams `--message-format=json`, and cargo writes `build-finished` before it
spawns its first test runner.

The lane reads that stream up to `build-finished` and, for that resolution,
makes the comparison a rebuild would:

- test executables by path and content hash, under the lane's target
  selectors
- the resolution's own runtime-index fingerprint. Each resolution is its own
  `cargo test`, so the plan keeps one per resolution beside the merged one.
- the support executables, hashed again from their recorded descriptors

Until that verdict is in, the harness shim answers no connection, harness or
rustdoc: the comparison reads live files, and a test that ran first could
change them. A strict session then admits only a clean build. A drifted,
unverifiable or failed comparison refuses every connection of that resolution
before any of its tests runs, and the lane fails with the reason. A
non-certifying session admits whatever the verdict. A drift there supersedes
the lane, journaled the moment it is found, whether or not anything ever
connects.

A connection that waits 30s with no `build-finished` read settles the gate as
unverified, and no later verdict overwrites it. A non-certifying lane is
superseded right then, before that connection is answered. A stop or a closing
session ends every wait, a settled gate included. A hash that can't be taken
is the lane's error in either mode.

The barrier is per resolution: a package-mode lane's earlier resolutions have
run, each against its own verified build, when a later one is refused. The
comparison itself runs on the stdout drain, so the phase watchdog bounds it,
not the connection wait. The prepare build of such a lane is the run's
cargo-side argv exactly, `--tests` or not (`serial_target_args`), so the two
streams describe the same units.

The `build_packages` support builds run again first only when the lane's
snapshot shows a planned file changed, since `cargo test` never rebuilds a
support package outside its selection. A lane whose harnesses can't go
through the shim (see "When the shim can't be installed") has nothing to hold
its harnesses at, and takes the rebuild path.

### Rebuild path

The rebuild path applies when a file no longer matches, for a serial lane with
no shim, and for a lane prepared without a snapshot. The lane's support builds
and its shape are built again, re-uplifting the support binaries a later
lane's build may have replaced. Once **every** resolution of the lane is
rebuilt, brokkr checks three things against the plan:

- the test executables, by path **and** content hash
- the runtime-index fingerprint
- the support builds' executables

brokkr waits for every resolution because one resolution's build can rewrite
another's binary in place. A shape whose difference cargo doesn't hash into
the file name (an env var a build script reads) rebuilds the same path in
place, so only the content can show that the binary there is another shape's.

Under a complete claim any difference is a hard error for the lane,
"artifacts changed since the plan", never a silent re-plan. Nothing builds
between that check and a direct lane's launches. A run that certifies nothing
isn't refused for a plan that went stale. The lane runs what it builds now,
the journal records the lane as superseded, and its inventory reads
unavailable with the reason.

On a strict session, each serial harness is also hashed again at its shim
handshake, after the gate and immediately before it runs, and refused if it
isn't the planned content. The contents of link-search directories (a dylib a
build script dropped there) aren't verified, since no artifact stream names
them.

The lanes then **consume** the prepared selection. The parallel lane claims
slots from the plan's counts, the isolated lane runs the plan's cases, and the
engine lane executes the very `TestList` it was prepared with.

### Unattributable lane shapes

Two lane shapes can't be attributed, so a complete profile refuses them during
`prepare`, with the reason:

- a serial lane whose harness shim can't be installed (see "When the shim
  can't be installed")
- cargo-mediated parallelism (`test_threads` 0 or above 1), which shares one
  stream across tests and harnesses. Use `parallel = { budget = N }`, which
  runs the binaries concurrently and attributably.

The second is config-determined, so config load refuses it first, naming the
profile and entries (`docs/brokkr.toml.sweeps.md`). The `prepare` refusal
stays as the backstop. A profile that certifies nothing keeps both lane
shapes, and their inventory is unavailable.

### Execution accounting

The runners report what they see, typed, as they see it:

- each libtest record: suite started and finished, test started, test
  finished ok, failed or ignored, and libtest's own `test/timeout` slow
  warning
- how each stream ended
- each kill and why
- each directly launched process's exit
- every attribution failure

The runner knows no plan. The lane resolves each stream against its own plan,
by the executable a harness's handshake named or the binary a direct lane
launched, and journals the result. Text the runner synthesizes for display,
such as a dead suite's closure, is never evidence. Each expected execution
ends with one outcome:

| Outcome | Evidence |
|---|---|
| `passed` | a terminal `ok` for that execution |
| `failed` | a terminal `failed`; or an attributable execution failure: a spawn error, the crash of a one-test process |
| `timed_out` | this execution crossed its own deadline, attributed: a one-test process killed by any clock, or the per-test cap naming a watched in-flight test; any engine timeout |
| `interrupted` | started, no terminal, ended by a termination it didn't cause (the phase deadline, a sibling's timeout, fail-fast, an interrupt) or by a stream that ended without its terminal |
| `ignored` | an explicit ignored result |
| `unobserved` | expected, never started, never finished |

Each outcome has a detail: `assertion`, `signal`, `spawn_error`,
`per_test_deadline`, `run_deadline`, `phase_deadline`, `sibling_timeout`,
`fail_fast`, `stream_truncated`, `read_error`, `interrupt`,
`missing_terminal` or `engine_error`.

### Termination rules

Each kind of termination resolves the executions it touches:

- **Phase deadline**: a run failure. Active executions are `interrupted`,
  queued ones `unobserved`.
- **Per-test timeout**: the attributed execution is `timed_out`, the others in
  its process `interrupted` (`sibling_timeout`), and the run stops, so
  everything after it is `unobserved` for that reason (`sibling_timeout`). The
  deadline is charged to the execution, its test **in its binary**, never to
  a name. On the isolated lane, binary A's `tests::foo` timing out leaves
  binary B's queued `tests::foo` `unobserved`, not `timed_out`.
- **Idle, no-completion or wall** with no defensible identity: a run-level
  timeout (`run_deadline`), and the affected executions stay unresolved. No
  timed-out test is ever invented from a suspect.
- **Fail-fast**: every later lane the test phase would have attempted is
  recorded as stopped by it, so its executions are `unobserved` for a stated
  reason. A lane the selection excluded (`-p`, dedup, not applicable) expects
  nothing and gets no record, so it's never named as the stop's scope. The
  parallel lane's sibling cancellation, and the engine lane's, are recorded as
  what they are, never inferred from a signal status.
- **A stream brokkr stopped reading** (the drain grace ran out with something
  still holding the pipe) is told apart from one its writer closed and one a
  read error ended. The first marks its unfinished executions
  `stream_truncated`. A truncated stream, or an attributed stream that never
  recorded how it ended, also makes the accounting `incomplete` on its own,
  whatever its executions' terminals say. What followed the cut (a duplicate,
  an unplanned test, the suite total) is unknowable, so the record can't be
  whole.

### Anomalies

Within one stream, these transitions are **anomalies**. Each fails the green
invariant, and none is ever deduplicated away:

- a duplicate start
- a start after a terminal
- a repeated terminal, even two identical `ok`s
- a record for a test the plan doesn't expect from that binary
- a suite starting inside an open suite
- a stream its writer closed with the suite still open and nothing recorded
  to explain it
- suite totals that disagree with the individual records
- libtest's slow-test warning (past 60s, under a 20s cap)
- test records on a stream the plan can't attribute
- an attribution error from the shim

Across streams there's one more: an execution reported by a second stream
(`duplicate_execution`, see "Identities"). A terminal record without a start
still records its outcome.

A test's identity on a stream is its **slot and name**. The slot is the suite
it was reported in, opened only by a valid suite start, so an overlapping
start keeps the open suite's slot; or outside any suite. The transitions above
are judged within one slot. Cargo's own stdout (`StreamSource::Cargo`) has one
binary's suite after another's, and two binaries may each hold a test of the
same name, so there a name may recur in a later slot. On a stream that has one
binary (a directly executed harness, an isolated one, an engine binary), the
same recurrence is that harness running the test twice (`name_across_slots`),
live and on replay. A lookup by name answers only when exactly one slot holds
it, finished or not. Replay treats an unattributed stream as cargo's; any test
record on one is already an anomaly.

### Test record lines

A verdict can fail on its record rather than a red test: a protocol issue, an
attribution refusal, an engine failure. It can also fail on an unsuccessful
exit that no kill and no red test explains; a harness's own red test explains
its exit, and any red test explains cargo's. In those cases the failure output
names each cause as a `test record:` line, beside any red test, on the cargo,
parallel and isolated lanes. Cargo's own summary can read all green while the
record fails the sweep.

An exit line names the tests a death left unfinished, its suspects. Cargo's
own exit is named only when no harness stream explains it, and a run that saw
no test at all (a failed build) has its own report. `brokkr test` prints the
record lines the same way, and reports an unsuccessful process with no red
test as a harness failure in cargo's words, with its suspect.

### The green invariant

**The green invariant** holds when all of these do:

- the plan is complete
- every expected execution `passed`
- nothing anomalous was seen
- no termination is on the record
- every planned doctest carrier completed (see "Doctest accounting")

`interrupted` fails it, and so does `ignored`, since an expected execution is
one the lane promised to run. A recorded termination fails it even when every
execution had already passed: a stopped run isn't a gate. A test phase that
went green but violates the invariant is a hard failure of the check, with
every execution that didn't pass listed.

Exit codes are unchanged: a fired ceiling exits 124, an interrupt 130, any
other failure non-zero. The verdict reads the stop flags themselves, not the
phases' result. A ceiling that fires, or an interrupt that arrives, after the
audit accepted the journal leaves every later phase returning success, and the
run still ends `failed` with 124 (or 130), complete claim or not.

### Accounting status

The accounting status is one of three values:

- **`complete`**: every expected execution has a resolved outcome and nothing
  anomalous was seen. The record is whole, which on a failed run is all it
  says.
- **`incomplete`**: anything is unresolved, the plan is incomplete, the
  journal was cut short, or a stream was truncated.
- **`violated`**: an anomaly makes the record untrustworthy.

The audit is pure. It reads the journal back from disk and holds it to the
plan, and spawns, builds and arms nothing. That lets it run after a watchdog
kill, when the shutdown flag refuses every new process.

A fired phase ceiling writes its own run-wide `phase_deadline` termination
into the journal the moment it fires. It writes before it kills anything and
without taking the journal's lock, since a wedged thread may hold it. A run
whose main thread never unwinds (the backstop exit) therefore still records
why it stopped. That write is retried on `EINTR` and continued after a short
write. One that fails is reported on the forced error channel, and the audit
counts it as a journal error. Every journal record ends with its newline: a
trailing segment without one is a write that didn't finish, and the reader
reports it as a journal error rather than parsing it.

### Doctest accounting

**Doctests** have their own block. It records what the rustdoc streams said
(passed, failed, ignored) and, per planned doctest carrier, whether it
completed and how many rustdoc streams it presented. Doctests can't be
enumerated, so there's no inventory (`unavailable`) and no accounting
(`unknown`), never reported as zero.

A lane is a planned carrier only if it will actually run doctests:

- a `doc_only` lane, configured or derived
- outside a complete claim, which runs no doctests inline, a serial libtest
  lane under `[test] doctests = true` whose selection has no target selector.
  A target selector turns cargo's doctest pass off.

On a strict session, a rustdoc may connect only on a carrier's run; anywhere
else it's refused as an attribution error. Every rustdoc stream is journaled
with the cargo resolution of the run it came from. The plan holds, per
carrier, the resolutions that owe one (`doc_required`). For a configured
`doc_only` lane that's every resolution, since cargo refuses `--doc` with no
library. Otherwise it's each resolution whose selection reaches a member with
a doctested target.

A carrier **completed** when all of these hold:

- its lane finished with nothing recorded stopping it
- every rustdoc stream it presented was closed by its writer, with its suite
  closed
- every resolution it owed presented at least one stream of its own

One package's stream never stands in for another's. That's the one floor that
tells "ran" from "never ran"; how many more streams there should have been
isn't knowable.

The obligation comes from `cargo metadata`, never from the harnesses the build
produced: a target with `doctest = true` of crate type `lib`, `rlib` or
`proc-macro`, over the members each resolution selects. `[lib] test` and
`doctest` are independent, so a library harness neither implies doctests nor
is implied by them.

A carrier that didn't complete fails certification, the green invariant,
though it moves no binary accounting count, which is scoped to enumerable
binary tests. A failing doctest fails the test verdict through its cargo
status. The block names, per carrier, the owed resolutions that presented
nothing (`missing`) and the libraries a derived carrier declined
(`declined`, see "Rustdoc handshake"). A decline is policy evidence, journaled
as its own record, never a stream the carrier must close, and never counted
toward what it owes.

### Inventory and certification

Inventory and certification are separate. Preparation produces two products,
kept apart in code (`LaneInputs::certifying` is a different switch from
preparing at all):

- **The execution inventory**: the selected executions, the binary each lives
  in, a content hash (xxh3) of every test executable, and the journal the
  lane's observations go to. brokkr prepares it for **every enumerable lane of
  every run** (serial, parallel, isolated, nextest) before its first test
  executes, and for `brokkr test` (see "brokkr test"). The whole invocation is
  prepared up front, so a timeout in one binary can name the tests of the
  binaries and lanes after it. Doctests aren't enumerable and stay outside
  it.
- **The policy universe**: every test of each required shape, exclusions and
  dead filters. It exists only under `certifies = "complete"`, whose claim it
  is.

A run that certifies nothing therefore keeps the execution modes a complete
claim refuses, and says what that costs. A serial lane under cargo-mediated
parallelism (`test_threads` other than 1), or with no harness shim, can't be
attributed to binaries. Its inventory is **unavailable**, with the reason, and
no execution is invented for it. The only test names it can offer are the
suspects its shared stream blamed at a stop, listed as such, with no binary.

A binary that answers no libtest `--list` is kept as **unenumerable**, with
the reason. Its lane's inventory is `partial`, and the tests in it are outside
the inventory the way doctests are. If an unlisted binary's shared stream
names a test at a stop, the report lists that name under the partial lane as a
suspect, with no binary, as for an unavailable lane.

### Which binaries are listed

Which binaries preparation executes with `--list` follows what listed them
before preparation existed. The parallel, isolated and nextest lanes, a
complete profile's serial lanes, and its policy universe list **every** binary
by executing it, a `harness = false` target included. A custom harness that
answers with a valid libtest listing (libtest-mimic and its kin) is enumerated,
and its tests run like any other. One that doesn't fails the lane or the
enumeration cleanly ("did not produce a complete libtest listing"), never
contributing an empty set.

The one added exposure is a **serial lane under a partial profile**, which
runs its binaries only through cargo and still has an inventory. There, a
`harness = false` target is recognised from its package manifest, the same
classification `brokkr test` uses to leave such targets out of a focused run.
It is **never executed** to ask, since it's arbitrary code that may ignore
`--list` and run its real workload. It's kept as unenumerable with the reason,
and cargo runs it as usual. A manifest that can't be read leaves the same
unknown, and the binary is likewise not run.

### Preparation failures outside a complete claim

A lane that fails to prepare outside a complete claim is a lane with no
inventory rather than a refusal. Its failure is recorded, not retried: what
preparation printed is held back for the log. When the lane's turn comes, a
serial lane runs through cargo as usual, while a parallel, isolated or nextest
lane, which needs a selection to execute, reports the recorded failure instead
of preparing again. Preparing again would execute every binary's `--list` a
second time in one invocation.

The one re-preparation left is a lane whose inventory went stale because
another lane's build replaced an artifact after a *successful* preparation.
The shim stays permissive: the plan attributes the harness streams, it doesn't
fail closed.

### Preparation cost

On a partial check, preparing a serial lane costs, on top of the lane itself:

- the lane's `build_packages` support builds, twice: the warm-up, then the
  no-op authoritative build
- one `cargo metadata`
- two `cargo test --no-run` builds per cargo resolution, for the same reason
- two `--list` executions of every listable binary: the selection, then the
  ignored-only subset
- a content hash of every executable at the snapshot, just before the
  listings, and again after them

When the lane's turn comes and the harness shim is attached, the snapshot is
hashed again. The support builds rerun only when it no longer holds, and the
lane's own `cargo test` build is held to the plan from its stream, every
executable hashed once more. Without the shim, the lane takes the rebuild path
instead: the support builds and the same `--no-run` build run again, and every
executable is hashed again before the lane's own `cargo test`. That rerun is a
no-op build when nothing changed, and a support-bin re-uplift when another
lane's build replaced one. Every run writes a record.

### Ignored tests

Whether a lane runs `#[ignore]`d tests (`--include-ignored` or `--ignored`) is
resolved from the lane's **complete** launch argv: the sweep's own libtest
args and whatever was forwarded after `--`. `brokkr check -- -- --ignored`
lists ignored tests only, and they are expected to run. Resolving from the
sweep's args alone would read "exclude" and drop every one of them from the
inventory.

brokkr writes the journal for every run that has an inventory, closed
(`journal.jsonl` ends with its closing line) when the test phase ends. A
journal without that line was cut short, and the report says so.

### Retention

Every run keeps a record, but only the run in flight reads its own back (the
journal, to reconcile); nothing reads an older record. Records stay for
inspection after the fact, and only the newest few are kept
(`ACCOUNTING_RUNS_KEPT`). The run being opened is never pruned. A plan stores
each lane's pairs grouped by binary, not repeated per test, to keep the
per-run cost proportional to the tests rather than to their package ids.

### brokkr test

`brokkr test NAME` prepares the same way. Every sweep is built and its
harnesses discovered before the first test runs. Then one lane per sweep and
`-N` iteration records the matched executions in its journal. A doc-only
sweep's lane expects nothing. A timeout, a watchdog kill or an interrupt
prints the same report as a `check` run.

Preparing every sweep up front must not let one sweep's build stand in for
another's. Two feature sweeps sharing a `build_packages` executable would
otherwise run the first sweep's tests against the second sweep's CLI. So each
sweep is built **again** immediately before it runs: its support builds, then
its test build, a no-op that re-uplifts its support binaries. The rebuild is
held to the sweep's prepared record: harness content, runtime index and
support executables.

If it differs, the plan is superseded, as a partial `check` lane's is:

- the sweep is prepared anew and runs what is built now
- its lanes are journaled as superseded, so the report reads that inventory
  as unavailable, with the reason, instead of reading its observations against
  a selection it didn't run
- the log says so

The cost per sweep, on top of the sweep itself, is:

- one extra run of each of its support builds
- one extra `cargo test --no-run` build: a no-op rebuild when nothing changed,
  which re-reads the whole artifact stream
- a full content hash of **every test executable the build produced**, once
  when the sweep is prepared and once more at the recheck. That covers all of
  them, not only the harnesses holding a match, `harness = false` targets
  included.

On a package with large harnesses, the hashing is read time proportional to
their total size, paid twice per sweep.

## Parallel allocator rationale

The parallel lane's allocator weighs serial cost and claims proportionally for
reasons measured on real trees. Keep these constraints when changing it.

**Serial cost, specifically not wall time.** Storing each binary's measured
wall time oscillates. Wall time is a function of the slots the allocator
granted: a binary given a generous share finishes fast, is weighted cheap, is
starved next run, and becomes the pole again. Measured at a fixed budget on a
warm machine, that was a clean two-cycle. The pole alternated between the same
two binaries, the sweep swung between 13.8s and 19.5s, and the claim spread
collapsed run over run (1-7, 1-6, 1-4, 1-3) rather than converging.

Left running, the loop reached `1-1`, every binary at the floor. It took 55.8s
against a baseline of about 50s for not fanning out at all, so its terminal
state lost to the thing the lane exists to beat, while still reporting
success. Serial cost doesn't move when the allocation moves. Roughly
`wall = max(serial / k, slowest_test)`, so wall conflates what is measured
with what was chosen. Wall time is what you want to know, and for exactly that
reason not what you may feed back.

**Proportional claims, not test counts.** Taking as many slots as a binary has
tests, capped at the budget, silently defeats the lane on the workspaces that
need it most. Any binary holding at least `budget` tests claims the whole pool
and runs alone. A workspace whose binaries each hold more tests than the
budget then can't fan out at any sane setting. Overlapping two of them would
need a budget above their test counts, which is hundreds in flight: the
mistake the budget exists to make unspellable. It fails green too. On a
35-binary tree, eight fat binaries serialized at the full budget each and the
sweep reported success.

**The cache-domain default** is well defined on every vendor, but what it
*names* differs:

- **AMD Zen**: an L3 domain is a CCX, commonly a whole CCD of 6 or 8 cores,
  and a real boundary, since cross-CCD traffic leaves the die.
- **Intel**: there's no CCD, and L3 is shared across the ring or mesh, often
  across the socket.
- **Intel hybrid parts** (Alder Lake and later): P-cores and E-cores share
  the L3 and both are counted. That's honest for a cache-sharing question and
  slightly optimistic for a throughput one; sysfs exposes no portable
  core-class field, so the alternative would be guessing.

The count uses the CPUs in the selected L3 domain that the calling thread may
run on, so affinity can make it a subset on either vendor.
