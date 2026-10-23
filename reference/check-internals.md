# check and test internals

How `brokkr check` and `brokkr test` are built, and why: the parts you need in
order to change the pipeline safely. How the commands are used is in
`docs/commands/check.md` and its siblings `check.test-lanes.md`,
`check.hygiene.md` and `test.md`; the config schema is in `docs/brokkr.toml.md`,
`docs/brokkr.toml.conventions.md` and `docs/brokkr.toml.sweeps.md`. Sections
follow the order of the usage docs; a `###` section belongs to the usage `##`
section of the same subject (the time
ceilings, the clippy, rustdoc and test phases, coverage, diagnostic
continuation).

## Invocation selection

Which lanes each phase of `check` (and `brokkr test`) is eligible to attempt,
and with which packages, is decided once, before any phase runs
(`src/check_cmd/selection.rs`), and every consumer reads that answer: clippy
and rustdoc, the test phase, `prepare`, the lane runners (serial, parallel,
isolated, nextest), member-check, `brokkr clippy`, `brokkr test`, and the
`package rules` and `brokkr test` plan announcements. Nothing re-derives it.
It is invocation selection only: it never predicts binaries, tests, inventory
or outcomes. The prepared inventory and the runtime record (journal,
accounting) keep their own models.

Per phase, one entry per active sweep, in sweep order. The selection and
prepared-lane collections are `Lanes<T>` values built from the active sweep
list and addressed with `LaneIdx`; persisted lane records carry that same typed
index into the journal and termination labels:

- **Selection**, the effective package selection: the sweep's own `packages`
  list; `--workspace --exclude` from `test_exclude_packages` (test phase only -
  the exclusions never narrow clippy or rustdoc); no package flags at all
  (cargo's default selection); or an override - the CLI `-p` set (`check`,
  `brokkr clippy`), or `brokkr test`'s resolved package (whatever its source:
  `-p`, `[test] default_package`, the project default) - holding the packages
  the sweep's rules admitted. Package flags are spelled by one function of
  the selection.
- **Resolutions**: one combined resolution, or one per package under
  `feature_unification = "package"` - always exactly the selection's
  packages, in order, each once. Package mode over a selection that names no
  packages is a configuration error raised before anything runs (no
  `--json` trailer), never repaired into one unscoped resolution.
- **Disposition**: *eligible* (an attempt), *excluded* (the rules admit none
  of the `-p` packages), *deduped* onto an earlier eligible sweep of the
  same phase, *not applicable* (rustdoc's `doc_only` doctest carriers), or
  *disabled* (the phase does not run: a profile's `skip_phases`, the
  markdown-only shortcut, rustdoc without a `[rustdoc]` table - nothing is
  computed or validated for a disabled phase). Admission is exactly the
  package rules above, the exclusion list checked before the packages list.

Each eligible lane also carries its cargo **requests**, built with the
selection and stored on it: one per resolution (its package selection plus the
sweep's features projected onto it, see [Feature projection](#feature-projection))
and one per `build_packages` support build. Every cargo argv - clippy,
rustdoc, the pre-builds, the test runs, the prebuilds and listings, the
coverage enumeration, `brokkr test` - takes its package and feature flags
from a request, never from the sweep. The parallel lane's repro line for a
failing binary is the whole request that built it (its full package
selection and projected features) plus the binary's target selector, so it
resolves the prebuild's exact graph; under a multi-package selection a
`--lib` repro therefore runs every selected package's lib harness.

Dedupe differs by phase. Clippy and rustdoc fold a sweep onto an earlier one
that compiles the same thing: the same compile inputs (profile, rustflags,
env, unification) and the same requests - every run's selection (provenance
ignored), resolution boundary and projected features, and every support
build with its projected features. So two sweeps a `-p` narrows to the same
package and the same projected features are linted once. `check`'s test lanes
never dedupe: their filters and execution policies differ even where their
builds do not. `brokkr test` dedupes on what a lane would execute - the same
requests and compile inputs, with the effective debug answer (`--debug`/
`--release` over the sweep's pin over `[test] debug`) in place of the profile,
plus `doc_only` - so a sweep and its doctest twin stay two runs, and two
sweeps narrowed to the same package with the same build fold into one. The
coverage audit still groups by the configured shape (`build_shape_key`): it
identifies the declared sweep, and a complete profile refuses `-p` anyway.

A lane that is not eligible builds nothing for that phase. In particular
`prepare` records an excluded test lane as skipped and builds neither its
support binaries nor its shape; an eligible lane keeps the order support
builds, then its own build and listing, then the support fingerprint. Support
builds always use their own single-package selection (each `build_packages`
entry), never the lane's test selection.

**Nothing reached.** When the CLI `-p` set excludes every applicable sweep of
an enabled phase, that phase refuses: the refusal is decided with the
selection and reported at the phase boundary as a failed run with
`failed_phase` set and the `--json` trailer emitted. Clippy and rustdoc report
it as they start (`-p x: every sweep's config rules the selection out (a, b);
nothing reached clippy`). The test phase's refusal (`-p x: every sweep's
config rules the selection out; zero tests ran`) is reported **before**
`prepare`, under `failed_phase: "test"`, so a run that will test nothing
compiles nothing for it - which means it takes precedence over any
preparation, `cargo metadata` or plan-persistence error that run would
otherwise have hit. A disabled phase never refuses, nor does rustdoc when
every sweep is a doctest carrier.

**Reached.** The trailer's `sweeps` list is the union of two per-sweep flags:
one for the diagnostic phases (clippy and rustdoc share it) and one for the
test phase, each set when the phase starts work on that sweep - before cargo
returns, so a sweep whose run failed still counts, and a lane an earlier
fail-fast never reached does not.

Member-check has its own selection: not applicable (no `[member_check]` and
no `[bin] install` implying one, the cadence ruling the claim out, the phase
skipped), else eligible with the `-p` set. Which members exist is a cargo metadata fact, so the member set itself -
workspace members less `skip`, intersected with `-p`, skipped visibly when
that empties it - is resolved when the phase runs; which bins are eligible
stays cargo's runtime answer. Fixture-lint's selection is the entries whose
`when` admits the claim, independent of `-p`.

### How terminal the cap is

Two details decide whether "20 seconds" means 20 seconds:

- **The watchdog wakes on the deadline**, not on a fixed interval. It sleeps to
  the soonest thing it bounds - the oldest in-flight test's ceiling, the
  no-progress clock, the idle window, the caller's wall - capped by its poll so it
  still notices cancellation promptly. A flat 250ms poll meant every cap was really
  "the ceiling plus up to 250ms".
- **The run is frozen before it is diagnosed.** The offending group is SIGSTOPped
  first, then snapshotted, then SIGKILLed. Snapshotting first let the test keep
  running for as long as `/proc` collection took; killing first would fix the cap
  and destroy the diagnostic, since a dead process has no `wchan` and no stack.
  SIGSTOP stops execution immediately and leaves `/proc` readable.

The shared-stream, shim and parallel libtest watchdogs use one deadline
verdict, preferring per-test expiry, then idle expiry, then wall expiry, and
one freeze/snapshot/kill sequence. The shim measures idle time across the
whole session, including gaps between harnesses. Parallel wall expiry carries
the same snapshot and in-flight context as serial wall expiry. The shim also
honors the caller's `WallShape`: a resolved one-test invocation uses the
caller's name; a shared wall expiry lists in-flight names as context only.

### What the cap can and cannot prove

The cap is enforced from a clock fed by libtest's *announcements*, and those
share one stdout stream with whatever the tests write to it. A test can bypass
libtest's capture with ordinary safe Rust - `write!(std::io::stdout(), "x")` -
because libtest installs Rust's capture mechanism rather than redirecting the
process's OS descriptor; a subprocess started with `Command::status()` inherits
the descriptor outright. Output with no trailing newline gets libtest's next
record concatenated onto its line (brokkr recovers that case, see
`split_trailing_event`), and a record that still goes unparsed is a lost event.

Because those records also *refresh* the clocks, a test that emits them could
otherwise buy itself unlimited time - re-emitting one completion on a timer would
reset the no-progress clock forever. So a refresh is counted **once per test
name**: a forger can spend only as many refreshes as there are distinct names it
invents, and a replayed record buys nothing.

The residual limit is worth stating rather than glossing: on a shared harness the
cap is enforced through a stream the tests can write to, so a determined test that
invents a fresh plausible name every twenty seconds is bounded by the sweep
backstop rather than by twenty seconds. An unforgeable per-test cap needs the
process to be the unit of one test, which is what the isolated lane, the nextest
lane and `brokkr test --timeout` give you.

That affects the **name**, not the entitlement to kill. If a test's terminal event
is lost, either that test really has burned its budget, or brokkr has seen no test
complete for longer than the budget - and the run has exceeded what the contract
allows either way. So the cap fires regardless, and when brokkr knows its
attribution is a guess it says so in the report rather than presenting a suspect
as proven.

Where the process is the unit of exactly one test - `brokkr test --timeout`, the
isolated lane, the nextest lane - the name is the caller's selection rather than
anything parsed, and the attribution is exact as well.

### An absence is never a fact

Every verdict path is fail-closed on missing evidence, because a zero and a
silence look identical in a counter. A `cargo test` that exits 0 having reported
nothing is not a green run - it is a run brokkr cannot vouch for - and the shapes
that reach it are ordinary: a test that prints and terminates the harness, an
abnormal exit after a suite announced itself, a custom harness that ignores the
flags.

So `ParsedTestResults` carries `Completeness`, set by comparing suites that
announced themselves against suites that summarised. They must agree exactly, and
zero-of-each is `Incomplete` rather than `Complete`: silence is not agreement.
Every consumer checks it before reading a count as a fact -
`brokkr test`, the `check` sweep report, the isolated lane, and the parallel lane
(which additionally cannot infer a pass from a clean exit, because it derives its
count from observed completion events).

The same rule governs enumeration. A listing with no `N tests, M benchmarks`
tally is not an empty listing, and a cargo artifact stream with no
`build-finished` record is not an empty selection. Both fail rather than
contribute an empty set, because the coverage audit certifies over exactly those
sets - and an audit that attests to nothing while printing `0 pairs, 0 orphaned`
is worse than one that refuses, since a green audit is taken as evidence.

### The clocks, per lane

`Ceilings` in `src/test_runner/deadline.rs` carries the cap, an optional wall
backstop, and a `WallShape` saying what a kill may be *called*. The numeric
bounds live in `src/test_runner/ceilings.rs`. Every lane enforces the same 20s
cap; they differ only in how exactly a kill can be attributed.

| lane | cap | kill named by |
|---|---|---|
| `check` serial sweep | 20s | tracker's best-known suspect |
| `check` parallel sweep | 20s | tracker's best-known suspect |
| `brokkr test <NAME>` | 20s | tracker's best-known suspect |
| `brokkr test --timeout N` | N, max 280s | caller's resolved test name |
| isolated lane | 20s | caller's selected test name |
| nextest lane | 20s (engine `slow-timeout`, `terminate-after = 1`) | engine, process-per-test |

Two further clocks bound what the per-test cap cannot charge to any test:

- **Idle ceiling** (5 min, `IDLE_TIMEOUT`): no test in flight at all, so there is
  no budget to spend. This is the case it was built for - cargo parked on
  `Blocking waiting for file lock on build directory` because another cargo held
  the target lock, with no test ever announced.
- **Wall backstop** (`SWEEP_WALL_TIMEOUT`, 30 min): a wedge neither clock above
  can attribute. The 15-minute `test` phase ceiling - `check`'s, and
  `brokkr test`'s per sweep and per iteration - always fires first, so this is
  the bound only for a caller that arms no phase watchdog.

One more bound sits after the clocks stop: **output drain grace** (2 s,
`DRAIN_GRACE`). The cargo streams and each isolated harness stream use the same
join policy: after cargo (or a directly run test binary) exits, drains get that
long to reach EOF. A process the tests left running that inherited a pipe can
keep it open indefinitely. Past the grace the drains are cancelled and joined,
a warning says capture stopped, and the run goes on with what arrived. Nothing
is signalled - the run's process group may already be gone and its id reused; a
leaked process carrying the run token is reaped at the next hold (see Strays).

The time limits are indexed in `src/test_runner/ceilings.rs`; phase ceilings
are selected by `Phase::ceiling`, and the check watchdog applies the whole-run
bound. Output drain grace remains in `src/test_runner/procgroup.rs`. There is
no config key or flag for these limits.

## Strays

The ceilings bound the damage; the reap removes the cause. brokkr's premise
is that every cargo invocation on a development host goes through it - that
is what the global lock serializes and what the measurement stores assume. A
cargo running outside brokkr competes for CPU with whatever is being measured
and takes cargo's build-directory lock, on which brokkr's cargo then blocks
with nothing to time out. Both hangs above were this: a rust-analyzer `cargo
check` (a nightly regression, September 2026) held the target lock for over an
hour, and the next day four of its build scripts spun at 100% CPU under it.

A fresh locked hold reaps unowned cargo-family processes after acquisition.
`src/stray.rs::is_cargo_family` owns the executable classification; the reaper
then applies its process-identity and ownership checks, kills leaves first, and
reports comm counts plus deduplicated starters on one `[strays]` line. The
live per-process detail is `brokkr strays`' job. The starter is the nearest
ancestor outside the family. When it is rust-analyzer it is killed too, because
it would only restart the cargo; a shell or editor starter (a hand-typed
`cargo`) is never signalled. Failure to read `/proc` reads as nothing found:
the reap is on the way to the real work, never a gate on it.

Formatters are exempt: `cargo-fmt`, and a `cargo` whose first positional
argument is `fmt`. `brokkr fmt` holds no lock unless `disable_toolchain` is
set, so its cargo has no verified owner, and every locked command elsewhere on
the host used to SIGKILL it. `cargo fmt` takes no build-directory lock and
compiles nothing, so it is outside what the reap exists to stop.

**Both reapers spare the same process-tree boundary: this brokkr, its visible
ancestors, and its visible descendants.** `ReaperExemptions` in `src/stray.rs`
is also used by the orphan selector. A token-bearing ancestor of a live brokkr
and a cargo process that launched it survive either reap. An ancestor Cargo
can hold the target-dir lock, and killing it could release that lock for nested
Cargo; it also SIGKILLs the Cargo operation that launched brokkr. The reapers
preserve that caller, so recursive Cargo under a build script can remain
blocked by its caller's target-dir lock.

**Stray ownership adds verified identities, never a name.** The original rule
exempted any process with a `brokkr` ancestor by `comm`, which made the
exemption spoofable by naming *any* executable `brokkr` - a shell copied to
that name exempted every cargo and rustc beneath it, with no reference to
whether it held anything. Beyond the shared boundary, the stray reaper
recognizes **the verified current lock holder** (checked through the lock
file's starttime and boot-id tokens) and its owned subtree.

A holder in **another PID namespace** (a sandboxed command, seen from the host)
cannot be verified by pid at all, so its namespace stands in for it: while it
holds, every process in that namespace or nested below it is treated as its own
(`NS_GET_PARENT` walk; a process that cannot be inspected is exempted). This
applies only when the holder's namespace differs from the reaper's - for a
holder sharing it, the rule would exempt every process on the host - and it
guards every target the reap would signal, descendants and starters included.
Without it, a hand-run `brokkr strays` on the host killed a sandboxed holder's
own cargo.

There is no name fallback. The case that argued for keeping one - never SIGKILL
brokkr's own build - is covered by the shared boundary, and the reaper can
identify its own process by pid. A hold whose identity cannot be verified from
this namespace therefore protects nothing beyond that boundary; previously it
fell back to the name, which let an executable called `brokkr` shield a foreign
compiler from a drain that then expired for want of killing it.

**The reap kills descendant trees, deepest first, and stops at exempt or owned
processes.** Signalling only the classified pids left a hole: killing a cargo
does not kill the rustc wrapper beneath it, so an already-admitted wrapper
survived and went on to exec a compiler. The shared boundary prevents selecting
the Cargo that launched this brokkr and stops expansion at brokkr's descendants.
A hand-run reap can still encounter another verified holder below a stray Cargo;
that holder and its subtree are skipped and not recursed through.

**Every signal is identity-checked.** A `Stray` carries the `/proc` starttime
recorded at classification, and the SIGKILL is sent only if that pid still has
that starttime. A bare pid is not an identity: between the scan and the signal
the process can exit and the number be reused, and the reaper would then kill
something it never classified. The lock code has paired pids with starttimes
since it was written; this path did not.

Containment is narrowed, not closed: a process can fork again between the `/proc`
read and the signal, and a lease holder can be a detached process with a name no
cargo-family scan matches. Closing that needs a cgroup, which is a larger change
than this reaper is.

The reap (and the stale-guard check below) runs inside `lockfile::acquire`
itself, once per *fresh* hold - never on a re-entrant acquire, whose outer hold
already reaped. It used to live in one command wrapper, and roughly a dozen
paths that took the lock directly (bench contexts, the pbfhogg verify harness,
piners, ratatoskr) silently skipped it.

A consequence worth naming: the post-acquisition reap runs only once the hold is
usable - after the drain - so it cannot rescue an acquisition that is itself
blocked. That is why the drain has its own reap on stall (see below).

`brokkr strays` is the by-hand form: bare lists, `--kill` lists then kills.
Works with no `brokkr.toml`. A legitimate run that needs
longer is a run whose sweeps want splitting, not a run that wants a longer
rope.

### Orphaned test processes

The stray reap matches by name, and a test binary is named after its crate. So
a second reap runs beside it at every fresh hold (`src/test_orphans.rs`) for the
one shape a name cannot find: test processes left behind by a brokkr that died
without running a handler - SIGKILLed by the OOM killer, say. The runners'
parent-death signal takes their direct child with brokkr, but on the serial
lanes that child is cargo, and the test binary beneath it (or a doctest under
rustdoc, or anything a test spawned) is reparented and keeps running.

Every process a test runner starts - libtest lanes and nextest lane alike -
carries `BROKKR_TEST_RUN=<token>`, one random token per brokkr process, and
inheritance carries it to everything below. The same process holds an
exclusive flock on `~/.brokkr/test-runs/<token>.lock` for its whole life. The
flock is the liveness proof: the kernel drops it however the process dies, and
it reads the same from every PID namespace sharing the filesystem, where a
recorded pid would mean something else. The reap takes each file's flock it
can get - a dead owner's - SIGKILLs every process whose `/proc/<pid>/environ`
carries that token, newest first (a child starts after its parent, so that is
leaves first), each checked against its starttime, and deletes the file once a
rescan finds no carrier left. A file whose carriers survive stays for the next
reap. A brokkr that exits normally leaves its file too; the next reap finds no
carrier and deletes it, and a process a test leaked past a clean exit goes the
same way as one left by a crash. The reap prints one line when it kills
anything, e.g. `SIGKILL sent to 2 orphaned test processes (my_crate-3f2a, sh)
that outlived the brokkr run that started them`.

Every failure is on the side of leaving processes alone: an unreadable
directory, a missing file, a flock held by anyone, an environment that cannot be
read (another user's process, or a test process that made itself
non-dumpable - which the reap therefore misses rather than kills). The reaping
brokkr uses the same `ReaperExemptions` boundary as stray cleanup and spares
itself, its ancestors and its descendants whatever token they carry. The mark
is inherited, so a brokkr started from inside a test carries the outer run's
token, as does every non-test child it starts. Killing is after the fact, not
at the moment brokkr dies: an orphan runs until the next locked brokkr command,
which is also when the stray reap would clear the rustc and rustdoc left under
the dead cargo.
`brokkr strays` does not list orphans; any locked brokkr command reaps them.
Unit tests never register a token or run the host reap; the mechanism is
exercised against a scratch directory and a child the test owns.

## The rustc guard

The reap removes strays after the fact; the guard prevents them from
compiling at all. `brokkr-rustc-guard` (`src/bin/rustc_guard.rs`, a second
bin target installed next to `brokkr`) is enrolled user-wide by `brokkr
guard --install` as `build.rustc-wrapper` in `$CARGO_HOME/config.toml`.
From then on every cargo this user runs - PATH-resolved or absolute-path,
rust-analyzer's, an agent harness's, a hand-typed one - routes each rustc
invocation through the guard. Cargo's startup `rustc -vV` probe goes through
the wrapper too, but probes are admitted (see below - refusing one poisons
cargo's probe cache), so a refused build dies at its first real compile
instead: zero crates completed, one refused.

### Admission is a capability for one acquisition

Admission is **not** an inference about the process tree. It used to be: a
`brokkr` ancestor in `/proc` passed. That rule was spoofable - any executable
*named* `brokkr` admitted every compiler beneath it, and a shell copied to that
name is enough - and it was an inference about process *shape*, so it failed
wherever shape is not visible: a PID namespace, a restricted `/proc`, a child
that reparents. Its failure direction was the worst available, refusing the one
process that must be allowed, because for brokkr's own builds the lock is always
held: brokkr *is* the holder. The comm walk is gone, not kept as a fallback.

Instead the holder marks its descendants. When a hold begins, brokkr mints a
random per-acquisition **nonce** (`src/hold.rs`) and hands it to every child it
starts, via the environment. Inheritance is the point: it reaches a grandchild
cargo spawned by a test binary or a script check without brokkr having to
predict which executables might eventually invoke cargo. The lock file carries
only the nonce's *hash*, so reading the world-readable lock file does not hand a
bystander a working capability.

The nonce is per **acquisition**, not per process. An identity built from pid,
starttime and boot id names the process, so one long-lived brokkr could take
hold A, spawn a child, release A, later take hold B, and the child's mark would
still match - admitted into a window it was never authorized for. A nested
(re-entrant) acquire shares the nonce; a genuinely new hold in the same process
mints a new one.

### The compile lease

Admission alone is not enough: a guard can be stopped between being admitted and
`exec`ing, then start a compiler inside a much later hold. So admitted
compilation holds a **reader lease** and a brokkr acquisition is the **writer**,
on a second lock file, `~/.brokkr/compile.lock`.

The guard takes a *shared* flock on `compile.lock`, clears `FD_CLOEXEC` so the
lock rides through `exec`, and only then reads the published hash. Lock first,
then check: a new hold cannot republish the capability without the exclusive lock
that the share excludes, so the value read cannot change underneath. Because
flock records live on the open file description, the lease is held by the
compiler itself and released by the kernel when the last holder of that
description exits - crash included, with nothing to clean up.

Acquisition, correspondingly, does this before the hold is usable:

1. Publish identity with `auth=` **empty** and `draining=1`. A guard reading an
   empty `auth` refuses, so nothing new is admitted while brokkr clears. That is
   the anti-starvation property, obtained from the publication order rather than
   from a separate intent flag a crashed writer could leave behind.
2. Take the **exclusive** `compile.lock`, draining earlier leases - those of
   compilers admitted under an earlier hold and of compilers started while no
   hold existed (an IDE check, a hand-typed `cargo`). After a short initial
   window the stray reap runs once, then the wait resumes under the configured
   drain budget. The window is short because every compiler
   still holding a lease at that point is a stray the post-hold reap would kill
   anyway; waiting longer only stalls the acquisition.
3. Mint the nonce and publish its hash *while still holding the exclusive
   lease*, so a guard paused before taking its share resumes, reads the new hash
   under its share, and cannot turn an earlier hold's nonce into an admitted
   compiler.
4. Release the exclusive lease so the hold's own descendants can take shares.

`compile.lock` is **never unlinked or replaced**. The proof that a drain
succeeded is an exclusive flock on that *inode*; a fresh inode at the same path
would silently split the exclusion in two.

**Acquisition can now fail.** If the budget expires, brokkr releases
`brokkr.lock` and reports that compilation leases did not drain - it does not
proceed into measurement. This is not a transient busy signal: a lease lives as
long as *any* process retains the descriptor, and a detached helper can extend
it without bound while carrying a name no cargo-family scan will match, so an
unchanged blocker fails every retry.

### The decision, and what it guarantees

In order: `BROKKR_CARGO` set passes, checked **before any lease** - a hatch that
needs working lock infrastructure is useless exactly when it is needed. Then the
lease is taken and the record read. Then the flock probe of `brokkr.lock` has the
final word on whether a hold exists at all: not held passes, whatever the file
says, while carrying the shared lease through `exec` so a later hold drains
this compilation. Held plus an empty `auth` refuses (a hold is starting up).
Held plus a non-empty `auth` admits only a matching capability.

The flock probe is last on purpose. Deciding from the record alone is a bug that
cost a probe run to find: abnormal termination leaves metadata behind while the
kernel releases the flock, so a crashed brokkr publishes a perfectly well-formed
capability that matches nobody - and refusing on it refused every compile on the
machine until something rewrote the file. A leftover record is inert; only a live
flock is a hold. Only `EWOULDBLOCK` demonstrates a holder; `EINTR`, `ENOLCK` and
friends say nothing and fail open.

The two sides fail in **opposite directions**, deliberately, because their
contracts differ. The *holder* fails acquisition if it cannot take the exclusive
lease or publish a complete record: brokkr must never run protected work
believing it has exclusion it never obtained. The *guard* fails **open** when the
lease infrastructure itself is broken - `compile.lock` unopenable, `ENOLCK`, a
read-only `$HOME`, `FD_CLOEXEC` unclearable - and says so on stderr. Fail-closed
there would let brokkr brick every build on the machine.

So the guarantee is **conditional**, and stating it precisely matters:

> Protected work excludes compilation that successfully participates in the
> lease protocol. Explicit overrides and guard infrastructure failures can
> overlap protected work.

A sandboxed guard denied access to `compile.lock` fails open and compiles
straight through a measurement, with no race required, and brokkr cannot even
reliably report that the guarantee degraded - the failing guard may be
elsewhere. The reap remains the backstop for non-participating cargo, and for
non-compiling cargo (`cargo metadata` is not fenced and does not need to be).

### Queries are admitted; refusing them poisons cargo's cache

Compiler-information queries - `rustc -vV`, and the `--print` batteries cargo
sends when probing a toolchain - are admitted unconditionally, before any lease
or lock question is asked (`classify` in `src/bin/rustc_guard.rs`). Two reasons,
and the second is the one that forced the rule:

- A query compiles nothing. The classifier parses the invocation's option
  grammar and accepts only shapes rustc answers before expansion and codegen:
  version requests, and `--print` requests from an explicit early-print
  allowlist, with no input beyond stdin's `-`. `--print=link-args` and
  `--print=native-static-libs` are compiles wearing a query's hat (rustc
  services them by compiling first) and stay gated, as does any option or print
  name the parser does not know. Misreading a query as a compile costs one
  refusal; the reverse would run real work outside the exclusion guarantee, so
  unknown shapes fail toward `Compile`.
- Cargo **persists a failed probe** - stderr and all - in
  `target/.rustc_info.json`, keyed by a fingerprint that is blind to inherited
  env vars, and **replays it without re-running the probe**. A foreign
  nonce-less cargo refused at its probe during anyone's hold therefore poisons
  the shared cache: every later cargo with the same fingerprint, brokkr's own
  correctly-stamped children included, fails with the guard's refusal verbatim
  while the guard never runs - the nonce is present and useless, and
  `BROKKR_CARGO=1` cannot cure it because it reaches a guard cargo no longer
  spawns. The poison heals only when a concurrent cargo holding a pre-poison
  in-memory copy rewrites the file (last-writer-wins), which is why the failure
  is intermittent. Demonstrated live 2026-09-10: a piners `cargo doc`
  script-check failed two runs with "carries no brokkr capability" despite a
  correctly-stamped child, then healed with no external change.

Query admission stops *new* poison at the source. Against *historical* and
*racing* poison, the shared spawn policy stamps
`CARGO_CACHE_RUSTC_INFO=0` (`hold::RUSTC_INFO_CACHE_ENV`), and nextest's
generated config carries it to engine-owned children. A poisoned entry
encountered *outside* brokkr is
cured by `CARGO_CACHE_RUSTC_INFO=0 BROKKR_CARGO=1 cargo <...>` - the first
variable makes cargo actually probe, the second makes the guard admit the
probe's compile siblings.

### Reserved environment variables

`BROKKR_HOLD_NONCE`, `BROKKR_COMPILE_LEASE` and `CARGO_CACHE_RUSTC_INFO` are
brokkr's transport. Project configs, `[[check]] env` blocks, and user-authored
build scripts or shell commands must not set, unset or override them. The
internal guard decision probe sets `CARGO_CACHE_RUSTC_INFO=0` for its isolated
Cargo build because that direct invocation bypasses brokkr's Cargo spawn
policy. The one hole `force` cannot close is a *workspace* crate's own
`build.rs` emitting `cargo::rustc-env=` for a reserved name - the nextest engine
applies the test package's build-script env after cargo's `[env]` table, so it
would displace the forced value for that crate's tests. The engine applies
build-script env only for the test binary's own package, so dependencies
cannot cause it. `BROKKR_CARGO=1` is different in kind: the deliberate human
escape hatch, set per invocation by a person and never by tooling.

### The version handshake

The guard answers `brokkr-rustc-guard --brokkr-guard-info` with a protocol
number, recognized before any lease or lock access so the answer comes even
when the lock infrastructure is wedged. brokkr compares it against
`guard::GUARD_PROTOCOL` at `brokkr guard` status and once per locked command
after acquisition, and warns - never fails - on a mismatch or a guard that does
not speak the handshake (an old guard treats the flag as the executable to wrap
and dies at exec, which the bounded probe reads as stale). The two binaries
install together, but nothing forces them to stay in step, and a half-upgraded
pair fails in shapes that read as anything but staleness. The number names
required *behaviour*, not wire format: bump it whenever admission semantics
change in a way brokkr depends on.

### Recursive acquisition is refused

A brokkr command started from *inside* an admitted compilation - a proc macro
that shells out to cargo, which is ordinary Rust - would take `brokkr.lock` and
then wait forever for an exclusive `compile.lock` that its own waiting parent
holds a share of. So the guard marks the compiler it admits
(`BROKKR_COMPILE_LEASE`), and a brokkr finding that mark refuses a *fresh* hold,
naming the deadlock. Nested (re-entrant) acquisition in the same process is
unaffected - that check comes first. Refusing loses that build; blocking loses
the machine. The mark is a convention, not a kernel fact: scrubbing the
environment drops it while leaving the descriptor open, and a helper can retain
it after closing its own - both of which fail toward a refusal rather than a
hang.

### A descendant of the holder refuses rather than waits

The same shape one level up: a locked brokkr started *beneath* a command that
holds the lock - `brokkr run brokkr -- test NAME`, a script check or a test that
shells out to brokkr - would wait on the flock while its ancestor waits for it
to exit, and the machine-wide lock stays held until someone kills both. So a
waiting acquisition compares the capability nonce it inherited (`crate::hold`;
every child of a hold carries that hold's) against the hash the holder
publishes, and on a match refuses at once, naming the holder and the remedy: run
the inner command outside the outer one (for brokkr itself, the built
`target/debug/brokkr`, or an installed one, invoked directly). Process ancestry
plays no part, so it works where a PID namespace hides the parent.

A match proves descent from the live hold, not that waiting would deadlock - an
ancestor could release and only then wait - so the refusal says what it knows.
An unrelated holder, a later hold after the inherited one was released, and a
holder still draining (which publishes no hash yet) are all waited for as
before. The wait polls (100 ms) rather than blocking in `flock`, so a record
that is momentarily unreadable while the holder rewrites it cannot commit the
waiter to an uninspectable wait, and a shutdown requested while waiting ends it.
Same-process re-entry is checked first and is unaffected. `brokkr run` keeps
holding the lock while its program runs - its serialization against benchmarks
and its `brokkr lock`/`kill` tracking are the point - so a locked brokkr cannot
run beneath it; that is the case this refusal reports.

The wrapper stays configured for brokkr's own builds too - never bypassed
by unsetting it - because the wrapper's identity is part of cargo's compile
fingerprint (cargo #9348): a wrapper that came and went per invocation
would ping-pong full rebuilds. Enrollment therefore costs one full rebuild
per target dir, once.

`[[script_check]]` children no longer need special treatment. A script-check is
arbitrary shell that may run cargo (`cargo doc --no-deps` is the usual one), and
brokkr does not own the process tree below `sh -c` - which is exactly why the
ancestor walk failed for it, observed as a `cargo doc` script-check refused at
its `rustc -vV` probe. It used to be handed `BROKKR_CARGO=1` explicitly; the
inherited capability survives detaching, re-execing and reparenting, so the
hand-granted override is gone. brokkr never grants `BROKKR_CARGO` to its own
children: that would let a script descendant keep compiling into later holds it
was never authorized for, which is the failure the per-acquisition nonce exists
to prevent.

`process::spawn::ChildPolicy` owns the shared spawn sequence: caller environment,
`cargo_env`, OOM protection, test/service parent-death and orphan policy, then
the hold capability last. The output runners use that policy, and its rendered
environment feeds nextest's generated config. Direct `Command` construction in
other subsystems is outside this choke point until those call sites migrate;
those paths must not be assumed to carry the hold capability or the rest of the
policy.

The nextest lane (`src/check_cmd/nextest_lane.rs`) launches test processes
through the linked engine's `TestList::new` and `runner.try_execute`, which
construct their own commands with no `Command::new` for the choke point to
catch. The capability travels the engine's documented route instead: each
`CargoConfigs` discovery owns a private, owner-only generated file containing
forced `[env]` entries from the sweep and the shared child policy. The engine
applies them to every process it spawns, including the rustc-info cache
disable. The file is removed after eager discovery, including on failure;
concurrent callers without a hold cannot replace another reader's environment.
The synthesized nextest profile uses the same private-file lifecycle.
A file rather than CLI `--config` expressions because the engine's CLI parser
accepts neither shape that can carry `force`: inline tables are rejected as
not-a-dotted-key, and split dotted keys fail because each argument
deserializes alone, leaving a `force` with no `value`. This closed what was
briefly a known gap between the capability landing and the overrides landing.

`brokkr guard` bare shows status (including a warning when the configured
guard binary is missing); `--install` writes the config line, resolving the
guard as the binary next to the running `brokkr` executable and refusing to
overwrite a wrapper brokkr did not write (sccache, say - chaining wrappers
is a human's decision); it also refuses to replace a guard from another brokkr
install. `--remove` can unset another exact-name brokkr guard too, including
one whose install is gone, but still refuses an unrelated wrapper. Works with
no `brokkr.toml`.

`scripts/guard-decision-probe.py` builds this checkout's
`brokkr-rustc-guard` in a persistent `brokkr-guard-decision-probe`
subdirectory of Cargo's resolved target directory, keyed by checkout path and
separate from check and test artifacts. It covers the capability paths against
a scratch `$HOME`.
It includes an idle machine, a stale record left by a crash, a draining hold,
a mismatched capability, no capability, the `BROKKR_CARGO` hatch, and query
admission against a refusing hold (with its compile-continuing-print
counterexample).
The `rustc-guard-decision-probe` script-check runs it at the `pre-test` stage,
so it does not depend on the binary installed by `brokkr install`. Run it after
any change to `decide()` or `classify()`: it is what caught
the stale-record refusal described above. The admitted path is covered end to end
by `brokkr check` compiling anything at all while brokkr holds its own lock; if
it were broken, no build on the machine would work.

## Platform tunables index

The constants stay with the subsystem whose behavior they shape. This index
points to those owners; their source definitions are authoritative, so the
values are not copied here.

| Area | Tunable owners |
| --- | --- |
| Child output and supervision | `STATUS_WIDTH` in `src/output/console.rs`; deadline polling, output-drain settling, capture limits, and forwarded-SIGTERM budget in `src/output/child.rs` |
| Lock acquisition and status service | lock polling and compile-lease drain policy in `src/lock/hold_protocol.rs`; request size, service polling, and request/reply budgets in `src/lock/service.rs`; stable lock-record read retries in `src/lock/record.rs` |
| Guard protocol | handshake probe budget in `src/guard.rs`; protocol fields shared with `src/bin/rustc_guard.rs` |
| Process cleanup | ancestry and descendant depth limits in `src/stray.rs`; orphan grace and reap policy in `src/test_orphans.rs`; child OOM adjustment in `src/oom.rs` |
| User-facing defaults | worktree retention in `src/worktree_record.rs`; short config-filter rejection in `src/config_parts/parser.rs`; `brokkr wc` default in `src/wc.rs`; man section-index threshold in `src/man.rs` |
| Check/test accounting retention | `ACCOUNTING_RUNS_KEPT` in `src/check_cmd/accounting/journal.rs`; the pruning selector accepts an explicit retention count for focused checks |
| External tool versions | release and checksum pins in `src/tools.rs` |
| Tilegen archive retention | `TILEGEN_RETENTION` in `src/elivagar/archive_store.rs` (the older `OUTPUT_RETENTION` name is no longer present) |

### A failure no diagnostic explains

A failed clippy (or rustdoc) run whose failure the diagnostic list does not
explain gets its captured streams after the list, narrowed by kind but never
capped by line count (`failed_run_streams`). Two cases qualify: the run left
no `error`-level diagnostic at all (a build script that panicked, an error an
allow filtered), or its stderr holds an error the list does not carry - under
`--keep-going` one run can report a rustc error in one crate and a build
script's panic in another, and hiding the panic until the rustc error is fixed
is the one-failure-per-run loop `--keep-going` exists to break. cargo's own
summaries of a failure reported elsewhere (`could not compile ...`,
`build failed`) do not qualify.

- stdout loses cargo's JSON records (a workspace emits one `compiler-artifact`
  record per unit, the bulk of the stream), except an error-level
  `compiler-message` the allow lists removed from the list, which keeps its
  rendered text - it is still a reason cargo failed.
- stderr, when it holds an `error` block, prints every error block the list
  does not already carry, and a trailer counting the warnings withheld
  (`script_check`'s `rustc` rule). A failed build script's `--- stdout`
  inside an error block loses its `cargo:` directives (one
  `cargo:rerun-if-env-changed=` per variable a `-sys` crate probes), counted
  on a `(N build-script directives not shown)` line; `cargo:warning=` and
  `cargo::error=` lines and the script's `--- stderr` stay whole. Any other
  block is withheld only when it is self-contained - every line after the
  header indented, blank, or a `help:`/`note:`/`=` continuation - and that
  test comes first, for listed manifest lints and cargo's bookkeeping lines
  too. A block that swallowed unindented text (relayed tool output with no
  header of its own) prints whole, since that text may be the cause. The
  trailer counts neither listed lints nor cargo's bookkeeping - the
  `generated N warnings` tallies, whatever their suffix, and `build failed,
  waiting for other jobs`. With no `error` block, stderr prints verbatim.
  Containment is a heuristic: foreign text that happens to be indented is
  still withheld with the block it follows.

### Lint-only mode

The phase reads rustdoc's diagnostics and never the site it renders, and
rendering - pages, search index, static files - is most of a `cargo doc`'s
time. When the rustdoc cargo will run lists `--check` in its
`-Z unstable-options --help` (a nightly; probed once per run,
`src/rustdoc_check.rs`), the phase adds `-Zunstable-options --check`, which
runs the same analysis and lints and writes nothing. A toolchain without it
renders as before; there is no key to set either way. The lints reported are
the same: a probe carrying one each of the default-on `rustdoc::` lints got the
identical warnings from both modes.

The flags ride cargo's rustdocflags, which, like rustflags, come from exactly
one source. A set `CARGO_ENCODED_RUSTDOCFLAGS` or `RUSTDOCFLAGS` is appended
to, never replaced; otherwise they go in as `--config build.rustdocflags`,
merged after the project's own entries. If a matching `target.*.rustdocflags`
outranks that layer the injection is inert - the phase renders, slower but
correct. `--commands` shows the placement either way.

Cargo judges a doc unit up to date by its output, `target/doc/<crate>/index.html`,
which `--check` never writes - so on its own every run would re-document every
member, changed or not. After each run the phase creates a placeholder there
(only if absent) for each unit cargo ran and sets its mtime to the moment
before cargo was launched, so an unchanged member stays fresh and a source
edited mid-run is still newer. A fresh unit's diagnostics are replayed from
cargo's cache, so a warning keeps failing the phase on an unchanged rerun. A
plain `cargo doc` carries different rustdocflags, so its fingerprint differs
and it renders over the placeholder. A wrong path guess (a `build.target` triple
moves docs under `<target>/<triple>/doc`) leaves the unit dirty - slow, never
wrong.

Sweep tags follow coverage, for clippy and rustdoc alike. A diagnostic is
tagged with the sweeps that reported it only when some run that selected its
package (by `-p`, or the default members for a bare selection; matched through
the message's `package_id`) did not. A shape-specific diagnostic - a link that
breaks only with a feature off - keeps its `[run-prep-adapter-free]`; one that
every covering sweep reports is untagged. A dependency member a run compiled
without selecting does not count as covered.

Output goes through clippy's parser and formatter: one line per diagnostic in
the `error[<lint>] <file>:<line>:<col> <message>` form, merged across sweeps,
sorted by lint with changed files first and nothing capped.

**Any diagnostic fails the phase, warnings included** - the rule clippy's gate
applies, and for the same reason. Rustdoc's lints are warn-by-default, so a
gate that failed only on `error` would pass every broken link unless the
project also set `-D warnings`. No `-D warnings` is injected: rustdoc takes
lint flags only through rustdocflags, where each change re-fingerprints the doc
units, and brokkr's own gate already fails on warnings.

For the same reason suppression happens at ingestion, not on argv:

- `[lints] allow` drops a diagnostic whose lint code equals an entry exactly
  (`rustdoc::private_intra_doc_links`). A group name matches nothing here,
  since a diagnostic carries its member lint's code. Prefer `Cargo.toml`'s
  `[lints.rustdoc]` for a rustdoc lint: brokkr's list also reaches the test
  phase's rustflags, and a new entry there costs a full test rebuild.
- `[lints] allow_exact` drops `lint@path` sites, as for clippy. The
  stale-entry notice splits by lint namespace: this phase judges the
  `rustdoc::` entries (`rustdoc: allow_exact ... suppressed nothing`), clippy
  judges the rest, so a clippy-only site that rustdoc never reports is not
  stale, and a `rustdoc::` site that clippy never reports is not either.

Skippable as `rustdoc` in a partial profile's `skip_phases`, skipped on a
markdown-only tree, and 5-minute ceilinged like clippy.

### Lint suppression in the test phase

`[lints] allow` and `[lints] allow_exact` are read by this phase too, not only
by clippy. They have to be: a lint that a project's `-Dwarnings` turns into a
compile error fails the *build*, and a suppression that cleared clippy but not
the test build left `brokkr check` green on one phase and red on the other for
the identical diagnostic, with no config key that reached the second.

Two things differ from the clippy phase, both forced by the fact that this
phase compiles rather than reads diagnostics:

- **`allow_exact` loses its file scope here.** The build fails during
  compilation, before any diagnostic reaches brokkr to be filtered, and `-A`
  has no path-scoped form - so each entry contributes its lint name build-wide.
  The run says so on its notice line.
- **Only rustc's own lints reach the build.** A `cargo::` entry is not passed
  as a rustc flag: rustc knows no `cargo` lint tool. Cargo's manifest lints
  still arrive on stderr and are reported as warnings by the shared owner
  path. Nor does a `clippy::` or `rustdoc::` entry:
  rustc raises neither, and rustflags are part of every unit's fingerprint, so
  an inert flag would only make the test build differ from a plain cargo's
  (or rust-analyzer's) in the same target dir - each then rebuilding the whole
  graph over the other's.
- **The flags travel through cargo's rustflags,** since `cargo test` has no
  `-- <rustc flags>` passthrough. Cargo picks exactly one rustflags source, so
  brokkr finds the layer already live for this build and adds there rather
  than exporting `RUSTFLAGS`, which would discard the project's own flags
  wholesale. The lookup covers matching target entries (including
  `CARGO_TARGET_<triple>_RUSTFLAGS`), `build.rustflags` (including
  `CARGO_BUILD_RUSTFLAGS`), and recursively included Cargo config files. It
  composes directly into the encoded or plain rustflags environment source,
  or uses `--config` at the active target/build layer. The notice line names
  inherited rustflags environment sources. See
  [`[lints]`](../docs/brokkr.toml.md#lints-section) for the full rule, including why
  an injection can be deliberately inert. If a live environment value is not
  UTF-8, brokkr refuses to compose it because the launch API cannot preserve
  it. If Cargo config cannot be fully read, the sink stays at the potentially
  inert build layer; Cargo reports its own config error before compiling.

The flags reach **every call site in the run that compiles**, not only the
`cargo test` invocation: the sweep pre-build (which would otherwise fail on the
unsuppressed lint before `cargo test` was ever reached), every lane's
prebuild, and the `prepare` phase's `cargo test --no-run` builds - each
lane's and each shape's universe. `brokkr test`'s own `build_packages`
pre-build carries them too. Both pre-builds also carry the sweep's pinned
`feature_unification`, like every other compiling path, so the binaries a
lane's tests spawn come from the same feature graph as the tests.

The universe build is worth naming because it is where the rule was learned.
When the enumeration ran last (it used to follow the test phase), a call site
missing the injection turned a run whose every lane went green into a failure
with no verdict - from the caller's side indistinguishable from a real one
until you read which phase the error came from. The test that guards it
asserts the property rather than the call site: enumeration is assembled from
the same selection the allows are prepended to.

### The failure list

A sweep's `cargo test` invocation runs several test binaries - the lib unit
tests, then one per `tests/*.rs` integration file, then doctests - and cargo
concatenates their output into one stream. Each binary prints its own pair of
`failures:` sections (the captured detail blocks, then the bare name list), and
the parser resets its section state at every `running N tests` line, so the
rendered `cargo test: N failures` list names failures from **every** binary,
not just the first one that failed.

That reset is load-bearing and has a regression test
(`failures_in_every_suite_are_reported_not_just_the_first`). Before it, the
state latched on the first suite: a failing lib test masked every integration
failure behind it. The exit code stayed correct, so the run was honestly red -
only the *which-tests* list was short, which is the worse shape of the two,
because a fixer who clears every listed failure concludes the run is
understood.

The reset alone was not enough, and the remaining three holes were closed
together after a downstream report (two agents in consecutive rounds read a
short list and mis-attributed which test carried a mutation's coverage):

- **The sweep passes `--no-fail-fast`.** Without it cargo stops after the
  first test *binary* that fails, so the later binaries never run and their
  failures cannot be reported by any renderer. An earlier note here claimed
  forwarding it "did not help"; that was measured against the latched-state
  bug, and it was never actually wired. `brokkr check` is a whole-tree gate,
  so it enumerates every failure it can reach in one run. A caller that
  passes its own `--no-fail-fast` is not double-flagged.

  `--no-fail-fast` needs **harness isolation** to be safe, and the serial
  lane provides it. In one shared libtest stream, a harness that dies mid-test
  (an abort, a stack overflow - what a mutation produces) leaves its suite
  open; the next harness's `suite/started` then arrives inside it and must be
  ignored, since a live test can print that record, and the per-test clock
  keeps billing the dead test until it kills the next, healthy harness as its
  hang. Nothing in the shared stream can tell a dead suite from a forged
  boundary, so brokkr stops sharing it: it installs itself as cargo's host
  target runner and as rustdoc (`src/test_runner/harness_shim.rs`). Each
  harness cargo launches is brokkr first - it hands brokkr the read end of a
  fresh pipe over a unix socket, puts the write end on its stdout and execs
  the real harness, keeping its pid and parent. brokkr authenticates each
  connection from the kernel (`SO_PEERCRED`'s pid, whose parent must be the
  root cargo and whose executable must be brokkr, once per process identity)
  and drains every pipe into its own reconstructor and tracker. A pidfd
  decides liveness: a dead harness stops being billed even if a leaked
  descendant holds its pipe open, while a live test that merely closed stdout
  is still billed. A run-level idle clock covers the gaps between harnesses
  (rustdoc compiling doctests after the last one included), so a gap is no
  longer charged to the previous suite. A harness that died with its suite
  open has that suite closed for it - its real failures rendered, the
  in-flight test printed as a **suspect** only - and cargo's own signal line
  becomes a `CRASHED <harness> (signal: ...)` entry in the failure list. That
  closure is display only: it never reaches the execution accounting, which
  reads the records the harness actually wrote. A connection that is not
  cargo's direct child (the doctest runtool under rustdoc, anything nested)
  runs unisolated, as before; the shim scrubs its environment from the program
  it execs, so a test that runs cargo itself gets the toolchain's own.

  Before it gets its pipe the shim says what it is: a one-message handshake
  carrying its role - `harness` or `rustdoc` - and, for a harness, the
  executable cargo asked the runner to run. Cargo runs `RUNNER <exe> <args>`,
  so the runner argv is the one place it states which test binary a harness
  process is, and the stream is attributed to that executable. Under a
  `certifies = "complete"` profile the session is **strict**: the executable
  must resolve against this lane and resolution's planned artifacts (a path
  planned for another lane does not satisfy it), a cargo child that fails the
  handshake or names an unplanned executable is refused and the refusal
  recorded as an attribution error, and the shim **fails closed** - it exits
  instead of exec'ing a harness on the shared stream. After a run cargo
  reports green, a planned harness with expected executions that never
  presented its stream is an attribution error too. Outside a complete profile
  the session stays permissive: a harness that cannot be isolated runs on the
  shared stream with a warning, as it always has.

  Where the runner cannot be installed without overriding something - a
  runner already configured for the host, a configured rustdoc, an ambient
  target other than the host (`CARGO_BUILD_TARGET`, `build.target`) on a sweep
  without its own `target`, a forwarded `--config` or `--target` - the sweep
  drops `--no-fail-fast`
  instead, warns why, and stops at the first failing harness: an honest short
  list rather than a wrong name. A complete profile refuses such a lane during
  `prepare`, with the reason: every harness would share one stream, and an
  unattributable stream cannot be accounted. The runner names brokkr as
  `/proc/<pid>/exe`, not its install path: cargo splits a runner string on
  whitespace, and a binary replaced mid-run (a `brokkr install` elsewhere)
  reads back as `<path> (deleted)`, which has a space and no longer exists.
  The magic link has no whitespace and still execs the replaced inode, and the
  shim authenticates its peer by inode rather than by link text. The threat model is accidents and misbehaving tests; project code
  that deliberately attacks this IPC is out of scope. When the cargo argv
  names a `--target` other than the host (a `[[check]] target` sweep), the
  shim is installed under that triple's runner variable as well as the
  host's: cargo runs the target's test binaries through the one and host units
  (a proc-macro's test harness) through the other.
- **The `failures:` name list is the authoritative roster.** The captured
  detail blocks are best-effort - an aborted suite can truncate the stream,
  and a detail block can hide inside another test's captured output. Any name
  the roster lists that the detail pass missed is appended bare (name, no
  location) rather than dropped. A suite that never summarised - its harness
  crashed after failing some tests - printed no name list at all, so for that
  one unclosed suite its `test NAME ... FAILED` verdicts stand in for one.
  Only for it: a verdict-shaped line a passing test prints into a suite that
  finished adds nothing.
- **libtest's `test result:` tally outranks the parsed roster for the
  headline count.** `cargo test: N failures` reports
  `max(tally, roster.len())`, and when the tally is larger the report says how
  many failures could not be attributed to a test name.
  Relatedly, an empty roster no longer routes to the compact "all passed"
  summary when the tally is non-zero.

Pinned by `the_name_list_completes_a_short_failure_roster`,
`the_rendered_report_names_every_failing_test` and
`an_unattributable_failure_is_counted_not_dropped`.

Unrelated but worth knowing beside it: a failing sweep ends the run before the
later `[[check]]` sweeps test at all, so their results are simply absent from
a red run. That follows from phase ordering, not from any reporting limit.

### Foreign manifest warnings

A passing test sweep and its support pre-builds parse compiler messages from
cargo JSON and manifest lints from stderr into `Diagnostic` values. `OwnerIndex`
classifies both package ids and manifest paths, so a warning from a path
dependency outside the workspace is dropped by the same rule used by clippy;
workspace member and root-manifest warnings still print. The remaining
warnings are rendered and held once per distinct diagnostic until the phase
ends (see "Output").

### Per-sweep rustflags

A `[[check]]` entry may carry `rustflags` (a token list, e.g.
`["--cfg", "madsim"]`), exported as `RUSTFLAGS` on that sweep's cargo processes
only - clippy, the test-phase pre-build, and the test run - and **composed**
with any inherited `RUSTFLAGS` (appended; `CARGO_ENCODED_RUSTFLAGS` is used
instead when the environment already carries the encoded form, which cargo
would otherwise let shadow `RUSTFLAGS`). brokkr sets no `RUSTFLAGS` of its own
(`--cap-lints=warn` is a cargo `--` arg, not a rustflag), so this composes only
with what the caller's environment carries.

Because a global cfg such as `--cfg madsim` reshapes every fingerprint in the
build graph, a sweep with non-empty `rustflags` **auto-isolates** into its own
target dir, `target/rustflags-<hash>` (the hash keys on the flag content).
Sharing the default `target/` would force a full recompile in both directions
every time the sweep alternated with the plain (default/ffi/live) sweeps;
isolation keeps the two caches apart, and `BROKKR_TEST_BIN_DIR` /
`CARGO_TARGET_DIR` are recomputed against the isolated dir. Sweeps carrying
*identical* flags share one dir, so several madsim legs compile the simulator
once. Isolation is automatic - there is no `target_dir` key; and setting
`RUSTFLAGS` or `CARGO_TARGET_DIR` in the entry's `env` alongside `rustflags` is
a parse error.

An entry may also carry its own `tests` / `skip` / `only` libtest filters,
**ANDed** with a referencing profile's filters of the same name (they append,
never replace). This expresses a curated per-package subset as several sibling
`[[check]]` entries under one profile - the shape a madsim gate needs, where
one crate runs a single named test and another runs a `virtual_time`-filtered
set, all under the same shared isolated target dir. See the `sim` worked
example in `docs/brokkr.toml.sweeps.md`.

#### Direct execution

Cargo does more than exec a test binary, so brokkr reproduces its launch
contract per binary (`src/check_cmd/direct_runtime.rs`, read against cargo's
`Compilation::fill_env`): cwd = the owning package's root; the dynamic-loader
path in cargo's order - the build-script link-search dirs that lie inside the
output dir (`target/<profile>`, found from where the executable sits, in either
the `deps/` or the newer `build/<pkg>/<hash>/` layout), sorted; the output dir;
the deps dirs; the toolchain libdir; then the inherited value (used alone when
it already starts with cargo's entries). A link-search dir outside the output
tree is left out, as cargo leaves it out, because it is likely to shadow a
system library. Then `[env]` from the cargo config chain with `force`/`relative`
semantics, `CARGO_PKG_*` / `CARGO_MANIFEST_DIR` / `CARGO_MANIFEST_PATH` from
cargo metadata (absent manifest fields export as empty, matching cargo),
build-script `rustc-env` values from the prebuild's `build-script-executed`
messages, runtime `CARGO_BIN_EXE_<name>` for the package's own bins, and
`CARGO` - the cargo executable itself, resolved through rustup when rustup is
the `cargo` on PATH, since cargo exports its own path, not the proxy's. **Not**
`OUT_DIR`: cargo exports it to build scripts and rustdoc only, never to a test
process it launches, so a runtime read of it fails under cargo and must fail
here too. A package whose build script ran more than once in the build with
different `rustc-env` output (built for two targets, or under two feature sets)
refuses the envelope, since the stream does not say which run the executable
linked against. Cargo-owned values are applied last, so a sweep's `env` cannot
forge them. Two refusals keep the direct launch honest: a configured target
runner (`[target.<triple>].runner` or `CARGO_TARGET_<TRIPLE>_RUNNER` - a
qemu/wine/valgrind wrapper direct execution would silently bypass), or any
ambient build target (`build.target`, `CARGO_BUILD_TARGET` - the host's
included, since setting one moves the artifacts), refuses
every direct-execution path (this lane, the process-isolated one, and `brokkr
test`) at resolution time - read from the environment cargo builds under, so a
sweep `env` setting either counts. A sweep's own `target` key is not an
ambient build target: see "Target sweeps" below. An unreadable or malformed selected Cargo
config, or a missing required include, also refuses launch reconstruction.
The chain is read once per lane, when its environment is decided, and that one
snapshot feeds the runner refusal, the serial shim decision and the direct
runtime - so such a config fails every lane kind, serial included, before it
builds.
Missing config search locations and missing optional includes are accepted as
absent. Forwarded cargo args that direct execution
cannot honour (`--no-run`, `--target`, `--config`, `--manifest-path`,
`--target-dir`) are rejected. Other cargo args shape the direct lanes'
prebuild, and target selectors narrow the prepared plan. Their
libtest args shape listing and selection. The parallel lane replays them for
each binary. The process-isolated lane keeps `--exact <case>` and
`--test-threads=1` on each child, so it applies positional filters during
planning and forwards option args to each one-test child; `--format` and
`--test-threads` remain runner-owned.
`CARGO_TARGET_TMPDIR` is deliberately not reproduced at runtime - cargo's
contract makes it compile-time (`env!`) only.

#### Target sweeps

A `[[check]]` entry's `target` is build shape (`EffectiveBuild` and
`DeclaredShape` encode it; the shape id encoding appends it only when set, so
every host shape id is unchanged) and `build_purpose::sweep_args` puts
`--target <triple>` on every cargo run of the sweep. Each lane resolves it
once, when its environment is decided (`resolve_sweep_target`,
`src/target_platform.rs`): the compiler cargo would run is found by cargo's
precedence (`RUSTC`, `CARGO_BUILD_RUSTC`, `build.rustc`, `rustc`) under the
sweep's env, and through it the sysroot must hold the triple's standard
library, `rustc --print cfg --target` gives the triple's cfg facts and
`--print target-libdir --target` its libdir; `cargo --version` must be 1.89 or
newer, since an older cargo builds a cross target's doctests and does not run
them. The cfg facts decide only names the triple alone fixes; every other name
is undecided, and an undecided atom makes its whole selector undecided.

Those facts feed three consumers. Runner discovery (`CargoLaunchConfig::
discover_for`) adds the triple's runner - its table, its env spelling, or a
`cfg(...)` table not proven false for it - and the lane refuses it in every
lane kind: brokkr executes the sweep's binaries itself (the serial lane lists
them too), so a runner would be bypassed, and emulated targets are therefore
unsupported. The same discovery drops an ambient `build.target`, which the
explicit `--target` overrides. The rustflags sink matches the triple's
`target.*` tables against those facts. And the direct runtime picks each
binary's toolchain libdir by its exact output root: `<base>/<triple>/<profile>`
is the target's, `<base>/<profile>` the host's, for `<base>` each of the
target dir and build dir cargo metadata reports under the lane's env; a root
matching neither, or both with different libdirs, refuses the envelope.
Forwarded `--target`, `--config`, `--manifest-path` and `--target-dir` are
refused on a target sweep in every lane, since each moves something the
resolution fixed.

Feature unification is unchanged on the prebuild side: on a sweep whose
selection is exactly the whole workspace (no `packages`, no
`test_exclude_packages`, no CLI `-p`, no package selector among the args
forwarded after `--` - every spelling cargo accepts, the attached `-pfoo`
short form included - and `default-members` is not a subset), the prebuild
and enumeration carry
`-Zfeature-unification --config resolver.feature-unification="workspace"`.
That promotion is what the default `feature_unification = "auto"` means, and
it keeps deciding which graph the binaries are *built* from - existing
configs compile what they compiled before. What direct execution retires is
any runner-side need for the pin, so an explicit `selected` on a
promotion-eligible sweep - formerly refused as undeliverable - is now simply
honoured (see [Feature unification](#feature-unification) below). A cargo too
old for the flag fails the prebuild with a named remedy; there is
deliberately no silent fallback. Consumer workspaces can get the one-graph
property for every cargo invocation (rust-analyzer and ad-hoc `-p` builds
included) by committing the equivalent `[unstable]` + `[resolver]` pin in
`.cargo/config.toml`.

Each binary claims a slice of the budget
proportional to its serial cost from the previous run - the sum of its own
tests' durations - floored at one slot, capped at its own test count, and
capped again where extra threads stop helping (`serial / slowest_test`). Costs
live in `.brokkr/parallel-timings.toml`; a tree with no history weights by test
count instead, so budget changes want two runs to measure rather than one.
The pure allocator combines these rules and orders binaries largest-first.
Serial cost is stored as integer milliseconds, not wall time, because wall time
depends on the slots granted, and feeding that back oscillates.
An unset budget (`parallel = {}`) resolves to the physical cores in one
last-level cache domain that the calling thread is allowed to use -
`brokkr env`'s `l3 domain:` line prints the number.

The lane writes two lines to the run log regardless of how many binaries run:

```
[run]     test fanout: 35 binaries, budget 24 in flight (claims 1-3)
[run]     test fanout: 4210 passed in 31.2s (35 binaries, built in 48.9s, slowest broadarrow-worker/ba-worker 28.7s)
```

On stdout its tests fold into the grouped test line, which names the slowest
parallel binary across every sweep with the sweep it came from.

The `claims N-M` spread is the diagnostic for whether the sweep actually
overlapped. Two binaries that each claim the full budget cannot overlap - the
failure mode the proportional claim rule exists to prevent, and one that
otherwise reports success - so that case alone prints, as a warning naming
the binaries (`N binaries each claimed the full budget (B) and could not
overlap`). Worded as what the claims prove: smaller binaries may still overlap
around them, so it does not claim the lane ran serially. A budget of 1
serializes by design and never warns.

A passing binary prints nothing of its own. Thirty-five green lines say only
what the summary says, and a reader who has learned to scroll past the normal
case will scroll past the abnormal one too. To watch one run, run it:
`brokkr test <NAME>` streams a single test's output live.
Build time and fan-out time are logged separately because a lane sold on wall
time must not fold the compile into the number it is judged on. **The slowest
binary is named because it is the sweep's floor** - the sweep finishes when it
does, so it is the one to split, or to move to the serial lane. The grouped
line carries the slowest across sweeps; each sweep's own floor is in the run
log, which is where a tuning session reads.

Failures are the exception: a failing binary prints its label, its
copy-pasteable cargo line, and its output. Every binary's output is buffered
and reported after the join in plan order, so concurrent binaries cannot braid
their failures together and two identical red runs produce identical output.

Mutually exclusive with `isolation = "process"` (refused before any test runs).
Doctests cannot run on a parallel sweep, so `[test] doctests = true` requires
at least one entry without `parallel` - and a `certifies = "complete"` profile
must include one among its own sweeps. Both are load errors.

Cargo target selectors passed after `--` (`brokkr check -- --test read_paths`)
shape which binaries the sweep plans, rather than being appended to each
per-binary command - cargo unions selection flags, so copying a selector onto
every invocation would run that target once per planned binary. Every
selector kind is honoured with cargo's meaning - `--test NAME`/`--test=NAME`,
`--bin`, `--example`, `--bench` (names may be globs), `--lib`, `--bins`,
`--examples`, `--benches`, `--tests`, `--all-targets` - and they union with
the sweep's own `tests` filters (`-- --lib` on a sweep with `tests = ["cli"]`
plans the lib harness and `cli`).
Full semantics, and the rule for partitioning a suite across a parallel and a
serial entry, are in `docs/brokkr.toml.sweeps.md`.

### Identities

- A **binary unit** is (package id, normalized target kind, target name) -
  the kind normalized the way cargo's selectors read it, `lib` for every
  library flavour (`rlib`, `cdylib`, `proc-macro`, ...). The executable path
  is launch metadata, not identity. nextest's binary ids map onto units
  through the lane's artifact index; reports name a unit by its nextest-style
  id (`pkg`, `pkg::target`, `pkg::bin/name`).
- A **pair** is (build shape, cargo resolution, binary unit, test). There is
  no conditional keying: two binaries of one package defining the same test
  path are two pairs on every lane (the old audit keyed libtest-only shapes by
  package and merged them).
- An **expected execution** is (lane, pair, attempt). The same pair selected
  by two lanes is two executions, each accounted on its own. No lane retries,
  so the attempt is always 1. The harness invocation is not part of the key
  but is enforced: within a lane a pair must run in exactly one harness
  process, and an execution belongs to the first stream that reports it - a
  start or terminal for it on any other stream is a `duplicate_execution`
  anomaly, and that stream's result (pass or fail) is never taken.

### The plan (`prepare`)

Before any test runs, every lane the test selection attempts is prepared (a
lane it excludes is recorded as skipped and builds nothing - see "Invocation
selection"): its declared support
builds (`build_packages`) run first, so a listing whose static constructor or
custom harness reads a support binary sees the one the lane runs with; then
its shape is built with the same `cargo test --no-run` the lane itself uses,
and every binary listed under the launch envelope execution will use
(`DirectRuntime::envelope`: cwd,
loader path, `[env]`, `CARGO_PKG_*`, build-script env), since a listing is
the binary's own code and a custom harness may read its environment. Under a
complete claim, each shape resolution's universe is enumerated. The plan
records, per shape resolution under that claim, its universe and
`#[ignore]`d subset; per prepared lane, its selected pairs and the executions
they imply (ignored names excluded unless the lane lifts `#[ignore]`;
filtered-out names and `#[bench]` functions are outside the claim); the
dead-filter findings (below); a content hash (xxh3) of every test
executable; the fingerprint of the runtime index the launch envelope is
built from (build-script env, out dirs and link-search dirs, and every
support bin behind `CARGO_BIN_EXE_<name>` by path and content hash) - on
every lane kind that executes binaries, the nextest engine lane included; the
executables each `build_packages` pre-build produced, by package, target,
path and content hash, hashed after the lane's own build (a support package
outside the test selection reaches the tests through `BROKKR_TEST_BIN_DIR`,
which no test build's artifact stream names); and an explicit marker for
anything that could not be prepared. A lane's
ignored-only listing is its filter argv with `--ignored` in place of any
`--include-ignored` (libtest refuses the two together).

Preparation has a build stage (`build_profile`) followed by lane inventory
and policy passes in `prepare_profile` (`src/check_cmd/prepare.rs`), because
a listing is binary execution under the same 20s wall limit as a test and
must never share the machine with a compile:

1. **Build.** Every attempted lane's builds - under a complete claim its
   universe builds first, then its `build_packages` support builds, then its
   shape - grouped by target directory as the clippy phase is: one chain per
   directory in configured order, the chains concurrent. Straight after each
   lane's builds, before the next build in its directory, the executables the
   lane runs or hands its tests are hashed - test executables, the runtime
   `CARGO_BIN_EXE_*` bins, the support executables. That is the lane's
   **snapshot**. A dylib a build script placed in a link-search directory is
   not in it, as no artifact stream names one (the same limit the rebuild
   comparison below has). Nothing is listed or executed in this step. The `background`
   script checks are joined at its end.
2. **Inventory.** Lane by lane in configured order, nothing compiling. A lane whose
   snapshot still holds is listed from its step-1 build; one that a later build
   in its directory replaced in place - or that has no step-1 build - is built
   again, alone, with its support builds, and listed from that build under a
   fresh snapshot, so every fact of the plan (support executables included)
   comes from the one build it is listed from. The snapshot is checked again
   after the listings, whichever build it came from: a file that changed
   while the lane's own binaries were being listed fails the lane, since its
   inventory would describe other binaries than the ones it runs. Each lane
   ends this pass with one inventory disposition: `prepared` (including any
   unlistable binaries), `skipped`, `unavailable`, or `doc_tests` for a
   doctest-only lane. A skipped lane has no selected executions, and an
   unavailable lane has no binary inventory.
3. **Policy (complete claims only).** After every lane's inventory disposition
   is fixed, enumerate each shape resolution's universe and `#[ignore]`d
   subset, validate that the prepared binaries belong to that universe, then
   judge filter liveness across the lanes. A skipped or unavailable inventory
   makes the relevant filters unknown, not dead. This pass writes the shape
   and dead-filter records; it does not alter the selected pairs.

Listings must not modify the files they are listed from. Hashing either side of
a listing catches a net change, not a file replaced and put back while it ran.

The plan is written to `.brokkr/accounting/<run id>/plan.json` (under the
config dir; retention is below) before the first test executes,
and a journal beside it, `journal.jsonl`, receives every observation as it
happens - one JSON line each, written straight through - so it survives
anything short of the machine going down, the watchdog's backstop exit
included. The journal's closing line is written when the test phase ends; a
journal without it was cut short, and the audit says so.

A lane that cannot be prepared - a build failure or a refusal - is recorded as
unavailable, and preparation goes on with the rest, so every refusal is
reported in one run. A stop (an interrupt or the phase ceiling) ends
preparation at once. Under a complete claim, either condition leaves an
incomplete plan (`plan_complete = false`) and the test phase does not run: a
record taken against an incomplete plan could certify nothing, and
`failed_phase` is then `"prepare"`. Under a profile that does not certify, a
lane that cannot be prepared is recorded as unavailable and still runs through
its normal lane path; the run can report its outcome without claiming a whole
inventory (see "Diagnostic continuation").

Before a lane that executes its binaries directly (parallel, isolated, the
nextest engine) runs, its snapshot is hashed again, with no cargo invocation.
If every file in it still holds the planned content, the lane runs its prepared
snapshot: the planned binaries under the envelope they were listed with.
Rebuilding here would only ask whether building again changes anything, and a
project whose build scripts rerun on every build answers yes even when nothing
changed - drift the lane never had, and on a complete claim a hard error. What
this claims is that the lane's snapshotted executables matched the plan
immediately before it ran; a test that replaces a support binary while the lane
runs, a detached process a build script left behind, or a dylib in a
link-search directory is outside it.

When a file no longer matches - and before every serial lane, and for a lane
that was prepared without a snapshot - the lane's support builds and its shape
are built again (re-uplifting the support binaries a later lane's build may
have replaced) and, once **every** resolution of the lane is rebuilt - one
resolution's build can rewrite another's binary in place - its test
executables are checked against the plan by path **and** content hash, the
runtime-index fingerprint against the planned one, and the support builds'
executables against theirs. A shape whose
difference cargo does not hash into the file name (an env var a build script
reads) rebuilds the same path in place, so only the content can see that the
binary there is another shape's. Under a complete claim any difference is a
hard error for the lane - "artifacts changed since the plan" - never a silent
re-plan. Nothing builds between that check and a direct lane's launches. (A
run that certifies nothing is not refused for a plan that went stale: the lane
runs what it builds now, the journal records the lane as superseded, and its
inventory reads unavailable with the reason.) The serial lane's `cargo
test` builds once more on its own, so each of its harnesses is hashed again
at its shim handshake, immediately before it runs, and refused if it is not
the planned content. Not verified: the contents of link-search directories
(a dylib a build script dropped there), which no artifact stream names. The
lanes then **consume** the prepared selection: the parallel lane
claims slots from the plan's counts, the isolated lane runs the plan's cases,
the engine lane executes the very `TestList` it was prepared with.

Two lane shapes cannot be attributed, so a complete profile refuses them
during `prepare`, with the reason: a serial lane whose harness shim cannot be
installed (a configured runner, a cross target, a configured rustdoc - see
"The failure list"), and cargo-mediated parallelism (`test_threads` 0 or
above 1), which shares one stream across tests and harnesses - use
`parallel = { budget = N }`, which runs the binaries concurrently and
attributably. The second is config-determined, so config load refuses it
first, naming the profile and entries (`docs/brokkr.toml.sweeps.md`); the `prepare`
refusal stays as the backstop. (A profile that certifies nothing keeps both
lane shapes; their inventory is unavailable.)

A preparation failure under a complete claim runs no tests. The audit then
says the test phase did not run and how many executions were held back,
rather than reconciling an empty journal into thousands of `unobserved`
executions and unfinished doctest carriers.

### Execution accounting

The runners report what they see, typed, as they see it: each libtest record
(suite started and finished, test started, test finished ok / failed /
ignored, libtest's own `test/timeout` slow warning), how each stream ended,
each kill and why, each directly launched process's exit, and every
attribution failure. The runner knows no plan - the lane resolves each
stream against its own (by the executable a harness's handshake named, or the
binary a direct lane launched) and journals the result. Text the runner
synthesizes for display - a dead suite's closure - is never evidence.

| Outcome | Evidence |
|---|---|
| `passed` | a terminal `ok` for that execution |
| `failed` | a terminal `failed`; or an attributable execution failure - a spawn error, the crash of a one-test process |
| `timed_out` | this execution crossed its own deadline, attributed: a one-test process killed by any clock, or the per-test cap naming a watched in-flight test; any engine timeout |
| `interrupted` | started, no terminal, ended by a termination it did not cause - the phase deadline, a sibling's timeout, fail-fast, an interrupt - or by a stream that ended without its terminal |
| `ignored` | an explicit ignored result |
| `unobserved` | expected, never started, never finished |

Each carries a detail: `assertion`, `signal`, `spawn_error`,
`per_test_deadline`, `run_deadline`, `phase_deadline`, `sibling_timeout`,
`fail_fast`, `stream_truncated`, `read_error`, `interrupt`,
`missing_terminal`, `engine_error`.

The termination rules:

- **Phase deadline**: a run failure; active executions `interrupted`, queued
  ones `unobserved`.
- **Per-test timeout**: the attributed execution `timed_out`, the others in
  its process `interrupted` (`sibling_timeout`), the run stopped - everything
  after it `unobserved` for that reason (`sibling_timeout`). The deadline is
  charged to the execution - its test **in its binary** - never to a name:
  on the isolated lane binary A's `tests::foo` timing out leaves binary B's
  queued `tests::foo` `unobserved`, not `timed_out`.
- **Idle, no-completion or wall** with no defensible identity: a run-level
  timeout (`run_deadline`); the affected executions stay unresolved. No
  timed-out test is ever invented from a suspect.
- **Fail-fast**: every later lane the test phase would have attempted is
  recorded as stopped by it, so its executions are `unobserved` for a stated
  reason. A lane the selection excluded (`-p`, dedup, not applicable) expects
  nothing and gets no record, so it is never named as the stop's scope. The
  parallel lane's sibling cancellation, and the engine lane's, are recorded
  as what they are - never inferred from a signal status.
- **A stream brokkr stopped reading** (the drain grace ran out with something
  still holding the pipe) is told apart from one its writer closed and one a
  read error ended: the first marks its unfinished executions
  `stream_truncated`. Either - or an attributed stream that never recorded how
  it ended - also makes the accounting `incomplete` on its own, whatever its
  executions' terminals say: what followed the cut (a duplicate, an unplanned
  test, the suite total) is unknowable, so the record cannot be whole.

Within one stream these transitions are **anomalies**, each failing the green
invariant and none ever deduplicated away: a duplicate start; a start after a
terminal; a repeated terminal (even two identical `ok`s); a record for a test
the plan does not expect from that binary; a suite starting inside an open
suite; a stream its writer closed with the suite still open and nothing
recorded to explain it; suite totals that disagree with the individual
records; libtest's slow-test warning (past 60s, under a 20s cap); test
records on a stream the plan cannot attribute; an attribution error from the
shim. Across streams, one more: an execution reported by a second stream
(`duplicate_execution`, see Identities). A terminal record without a start
still records its outcome.

A test's identity on a stream is its **slot and name**: the suite it was
reported in (opened only by a valid suite start, so an overlapping start keeps
the open suite's slot), or outside any suite. The transitions above are judged
within one slot. Cargo's own stdout (`StreamSource::Cargo`) carries one
binary's suite after another's, and two binaries may each hold a test of the
same name, so there a name may recur in a later slot. On a stream that carries
one binary - a directly executed harness, an isolated one, an engine binary -
the same recurrence is that harness running the test twice
(`name_across_slots`), live and on replay. A lookup by name answers only when
exactly one slot holds it, finished or not. Replay treats an unattributed
stream as cargo's; any test record on one is already an anomaly.

When a verdict fails on its record rather than a red test (a protocol issue,
an attribution refusal, an engine failure), or on an unsuccessful exit no kill
and no red test explains (a harness's own red test explains its exit; any red
test explains cargo's), the failure output names each one as a
`test record:` line, beside any red test - on the cargo, parallel and isolated
lanes. Cargo's own summary can read all green while the record fails the
sweep. An exit line names the tests a death left unfinished, its suspects;
cargo's own exit is named only when no harness stream explains it, and a run
that saw no test at all (a failed build) has its own report. `brokkr test`
prints the record lines the same way, and tells an unsuccessful process with
no red test as a harness failure in cargo's words, with its suspect.

**The green invariant**: the plan is complete, every expected execution
`passed`, nothing anomalous was seen, no termination is on the record, and
every planned doctest carrier completed (below). `interrupted` fails it, and
so does `ignored` - an expected execution is one the lane promised to run. A
recorded termination fails it even when every execution had already passed:
a stopped run is not a gate. A test phase that went green but violates it is
a hard failure of the check, with every execution that did not pass listed.
Exit codes are unchanged: a fired ceiling exits 124, an interrupt 130, any
other failure non-zero. The verdict reads the stop flags themselves, not the
phases' result: a ceiling that fires, or an interrupt that arrives, after the
audit accepted the journal leaves every later phase returning success, and
the run still ends `failed` with 124 (or 130) - complete claim or not.

The accounting status is `complete` when every expected execution has a
resolved outcome and nothing anomalous was seen - the record is whole, which
on a failed run is all it says - `incomplete` when anything is unresolved,
the plan is incomplete, the journal was cut short or a stream was
truncated, and `violated` when an
anomaly makes the record untrustworthy. The audit is pure: it reads the
journal back from disk and holds it to the plan, and spawns, builds and arms nothing - which
is what lets it run after a watchdog kill, when the shutdown flag refuses
every new process. A fired phase ceiling writes its own run-wide
`phase_deadline` termination into the journal the moment it fires, before it
kills anything and without taking the journal's lock (a wedged thread may
hold it), so a run whose main thread never unwinds - the backstop exit -
still records why it stopped. That write is retried on `EINTR` and continued
after a short write; one that fails is said on the forced error channel and
counted as a journal error by the audit. Every journal record ends with its
newline: a trailing segment without one is a write that did not finish, and
the reader reports it as a journal error rather than parsing it.

**Doctests** have their own block: what the rustdoc streams said (passed,
failed, ignored) and, per planned doctest carrier, whether it completed and
how many rustdoc streams it presented. They cannot be enumerated, so there is
no inventory (`unavailable`) and no accounting (`unknown`) - never reported
as zero. A lane is a planned carrier only if it will actually run doctests: a
`doc_only` lane, or a serial libtest lane under `[test] doctests = true` whose
selection carries no target selector (one turns cargo's doctest pass off).
On a strict session a rustdoc may connect only on a carrier's run; anywhere
else it is refused as an attribution error. A carrier **completed** when its
lane finished with nothing recorded stopping it, every rustdoc stream it
presented was closed by its writer with its suite closed, and - for a
`doc_only` lane always, for a serial carrier whose selection reaches a member
with a doctested target - at least one rustdoc stream presented at all (the
one floor that tells "ran" from "never ran"; how many more there should have
been is not knowable). The serial obligation comes from `cargo metadata` (a
target with `doctest = true` of crate type `lib`, `rlib` or `proc-macro`,
over the members the lane's `cargo test` selects), never from the harnesses
the build produced: `[lib] test` and `doctest` are independent, so a library
harness neither implies doctests nor is implied by them. A
carrier that did not complete fails certification - the green invariant -
though it moves no binary accounting count, which is scoped to enumerable
binary tests; a failing doctest fails the test verdict through its cargo
status.

### Inventory and certification are separate

Two products come out of preparation, kept apart in code (`LaneInputs::certifying`
is a different switch from preparing at all):

- The **execution inventory**: the selected executions, the binary each lives
  in, a content hash (xxh3) of every test executable, and the journal the
  lane's observations land in. Prepared for **every
  enumerable lane of every run** - serial, parallel, isolated, nextest - before
  its first test executes, and for `brokkr test` (below). The whole invocation
  is prepared up front, so a timeout in one binary can name the tests of the
  binaries and lanes after it. Doctests are not enumerable and stay outside it.
- The **policy universe** (every test of each required shape, exclusions, dead
  filters): only under `certifies = "complete"`, whose claim it is.

A run that certifies nothing therefore keeps the execution modes a complete
claim refuses, and says what that costs. A serial lane under cargo-mediated
parallelism (`test_threads` other than 1) or with no harness shim cannot be
attributed to binaries: its inventory is **unavailable**, with the reason, no
execution is invented for it, and the only test names it can offer are the
suspects its shared stream blamed at a stop - listed as such, with no binary.
A binary that answers no libtest `--list` is kept as
**unenumerable** with the reason: its lane's inventory is `partial` and the
tests in it are outside the inventory the way doctests are. If an unlisted
binary's shared stream names a test at a stop, that name is reported under the
partial lane as a suspect, with no binary, just as for an unavailable lane.

Which binaries preparation executes with `--list` follows what listed them
before preparation existed. The parallel, isolated and nextest lanes, a
complete profile's serial lanes and its policy universe list **every** binary
by executing it, a `harness = false` target included: a custom harness that
answers with a valid libtest listing (libtest-mimic and its kin) is enumerated
and its tests run like any other, and one that does not fails the lane or the
enumeration cleanly ("did not produce a complete libtest listing"), never
contributing an empty set. The one new exposure is a **serial lane under a
partial profile**, which ran its binaries only through cargo and now has an
inventory: there a `harness = false` target is recognised from its package
manifest (the classification `brokkr test` uses to leave such targets out of
a focused run) and is **never executed** to ask, since it is arbitrary code
that may ignore `--list` and run its real workload; it is kept as unenumerable
with the reason, and cargo runs it as it always did. A manifest that cannot be
read leaves the same unknown, and the binary is likewise not run.

A lane that fails to prepare outside a complete claim is a lane with no
inventory rather than a refusal, and its failure is recorded, not retried:
what preparation printed is held back for the log, and when the lane's turn
comes a serial lane runs through cargo as it always has, while a parallel,
isolated or nextest lane (which needs a selection to execute) reports the
recorded failure instead of preparing again. Preparing again would execute
every binary's `--list` a second time in one invocation. The one re-preparation
left is a lane whose inventory went stale because another lane's build replaced
an artifact after a *successful* preparation. The shim stays permissive: the
plan attributes the harness streams, it does not fail closed.

What preparing costs on a partial check, per serial lane, on top of the lane
itself: the lane's `build_packages` support builds; one `cargo metadata`; one
`cargo test --no-run` build per cargo resolution; two `--list` executions of
every listable binary (the selection, then the ignored-only subset); a content
hash of every executable. Then, when the lane's turn comes, the support builds
run **again**, the same `--no-run` build runs again and every executable is
hashed again (the verification that proves the lane is about to run what was
planned - each is a no-op build when nothing changed, and a support-bin
re-uplift when another lane's build replaced one), before the lane's own `cargo
test` builds a third time. Every run writes a record.

Whether a lane runs `#[ignore]`d tests (`--include-ignored` or `--ignored`) is
resolved from the lane's **complete** launch argv: the sweep's own libtest args
and whatever was forwarded after `--`. `brokkr check -- -- --ignored` lists
ignored tests only, and they are expected to run; the sweep's args alone said
"exclude" and dropped every one of them from the inventory.

The journal is written for every run that has an inventory, a closed one
(`journal.jsonl` ends with its closing line) when the test phase ends; a
journal without it was cut short, and the report says so.

### Retention

Every run keeps a record, but only the run in flight reads its own back (the
journal, to reconcile): nothing reads an older record. They stay for inspection
after the fact, and only the newest few are kept; the run being opened is never
pruned. A plan stores each lane's pairs grouped by binary, not repeated per
test, to keep the per-run cost proportional to the tests rather than to their
package ids.

### `brokkr test`

`brokkr test <NAME>` prepares the same way: every sweep is built and its
harnesses discovered before the first test runs (a sweep's discovery used to
wait for the sweeps before it to finish), then one lane per sweep and `-N`
iteration records the matched executions in its journal. A doc-only sweep's
lane expects nothing. A timeout, a watchdog kill or an interrupt prints the same
report as a `check` run.

Preparing every sweep up front must not let one sweep's build stand in for
another's (two feature sweeps sharing a `build_packages` executable would
otherwise run the first sweep's tests against the second sweep's CLI). So each
sweep is built **again** immediately before it runs - its support builds, then
its test build, a no-op that re-uplifts its support binaries - and held to its
prepared record: harness content, runtime index, support executables. If it
differs the plan is superseded, as a partial `check` lane's is: the sweep is
prepared anew and runs what is built now, its lanes are journaled as superseded
(so the report reads that inventory as unavailable, with the reason, instead of
reading its observations against a selection it did not run), and it is said on
the log. The cost, per sweep, on top of the sweep itself: one extra run of each
of its support builds, one extra `cargo test --no-run` build (a no-op rebuild
when nothing changed, and it re-reads the whole artifact stream), and a full
content hash of **every test executable the build produced** - hashed once when
the sweep is prepared and once more at the recheck, not only the harnesses
holding a match, and `harness = false` targets included. On a package with many
large harnesses the hashing is read time proportional to their total size,
paid twice per sweep.

### How the test phase injects them

Only lints rustc itself raises reach the test phase. A `clippy::` or
`rustdoc::` name is never raised by a rustc build, and a `cargo::` name is not
a rustc lint at all, so their allows stay out of the build. That is deliberate,
not tidiness: rustflags are part of every unit's fingerprint, so any flag here
makes the test phase's artifacts differ from what a plain `cargo build` or
rust-analyzer builds in the same target dir, and each then rebuilds the whole
graph over the other's. An allow for a rustc lint (`deprecated`, `dead_code`)
still pays that cost; where the project can say it in the manifest's `[lints]`
instead, every build shares one fingerprint.

`cargo test` has no `-- <rustc flags>` passthrough, so the test phase puts the
`-A` flags into cargo's rustflags. Which layer it uses is decided per sweep,
because cargo does **not** merge rustflags across kinds of source - it picks
exactly one, first match wins:

1. `CARGO_ENCODED_RUSTFLAGS`
2. `RUSTFLAGS`
3. every *matching* `target.<triple>.rustflags` / `target.<cfg>.rustflags`,
   joined together
4. `build.rustflags`

This is why brokkr does not simply export `RUSTFLAGS`: that promotes its own
flags to source 2 and discards the project's `-Dwarnings`, its
`-fuse-ld=lld`, its `relocation-model=pic`, all at once. (It is also why
`[[check]] rustflags` isolates a sweep into `target/rustflags-<hash>` and
should not be used to carry a lint allow.)

Instead brokkr finds the layer already live and adds to *that* one, where
cargo's own array merging applies and its entry lands last - which is what a
lint allow needs, since rustc resolves conflicting lint levels last-wins. A
config layer is reached with `--config`, the highest-precedence config source,
so the project's `.cargo/config.toml` is left untouched.

The chain inspected is the whole chain cargo reads: every `.cargo/config.toml`
from the build root upwards, plus `$CARGO_HOME/config.toml`. A user-level
`[target.<host-triple>] rustflags` is common - a linker choice, a
`-Ctarget-cpu` - and it silently decides the winning layer for **every**
project on that machine, including ones whose own flags live in
`build.rustflags` and are therefore already being ignored.

An unrecognised `cfg(...)` selector resolves to the inert direction, never the
destructive one: injecting at `build.rustflags` when a target table actually
wins merely does nothing, while injecting at `target."cfg(all())"` when
`build.rustflags` was the live layer would demote it and drop the project's
flags. The run always prints which layer it used, so an inert injection is
diagnosable rather than mysterious:

```
test: allowing deprecated via --config target."cfg(all())".rustflags ([lints] allow)
```

## Parallel allocator history

**Serial cost, and specifically not wall time.** An earlier version stored each
binary's measured wall time, which oscillated: wall time is a function of the
slots the allocator granted, so a binary given a generous share finishes fast,
is weighted cheap, is starved next run, and becomes the pole again. It showed
as a clean two-cycle at a fixed budget on a warm machine - the pole alternating
between the same two binaries, the sweep swinging between 13.8s and 19.5s, and
the claim spread collapsing run over run (1-7, 1-6, 1-4, 1-3) rather than
converging. Left running it reached `1-1` - every binary at the floor - and
took 55.8s against a ~50s baseline for not fanning out at all, so the loop's
terminal state lost to the thing the lane exists to beat while still reporting
success. Serial cost does not move when the allocation moves. Roughly
`wall = max(serial / k, slowest_test)`, so wall conflates what is being
measured with what was chosen: wall time is what you want to know, and
precisely therefore not what you may feed back.

The rule this replaced - take as many slots as you have tests, capped at the
budget - silently defeated the lane on the workspaces that needed it most. Any
binary holding at least `budget` tests claimed the whole pool and ran alone, so
a workspace whose binaries each hold more tests than the budget could never fan
out at any sane setting: overlapping two of them would have needed a budget
above their test counts, which is hundreds in flight - the mistake the budget
exists to make unspellable. It failed green, too: measured on a 35-binary tree,
eight fat binaries serialized at the full budget each and the sweep reported
success.

The cache-domain default is well defined on every vendor; what it *names*
differs. On AMD Zen, an L3 domain is a CCX, commonly a whole CCD - 6 or 8
cores - and a real boundary, since cross-CCD traffic leaves the die. On Intel
there is no CCD: L3 is shared across the ring or mesh, often across the socket.
The count uses the CPUs in the selected L3 domain that the calling thread may
run on, so affinity can make it a subset on either vendor. On Intel hybrid
parts (Alder Lake and later) P-cores and E-cores share the L3 and both are
counted, which is honest for a cache-sharing question and slightly optimistic
for a throughput one; sysfs exposes no portable core-class field, so the
alternative would be guessing.
