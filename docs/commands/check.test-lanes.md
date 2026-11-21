# Test lanes

This page covers the test-running half of `brokkr check`, and the lanes
`brokkr test` shares: how sweeps run, what a complete profile audits, and
what a failed run reports. The reference part is "test" and its lane
subsections, "nextest", "coverage", "Diagnostic continuation", "Feature
unification", "member_check", "Dead filters" and "Test environment". The
command, its output and sweep selection are in `brokkr man check`; time
ceilings are in `brokkr man hygiene`.

## test

The test phase runs after clippy, one lane per selected sweep:

- **Lint suppression.** `[lints] allow` and `allow_exact` reach every
  compiling call site of the run (pre-builds, prebuilds, `prepare`'s builds)
  through cargo's rustflags. `allow_exact` loses its file scope here and
  applies build-wide. Only rustc's own lints are passed; `clippy::`,
  `rustdoc::` and `cargo::` names never reach a build. See `[lints]` in
  `brokkr man config`.
- **The failure list names every failure.** The serial lane passes
  `--no-fail-fast` and isolates each test harness on its own stream, so a
  harness that crashes can't hide or misattribute the failures of the
  harnesses after it. A crash shows as `CRASHED HARNESS (signal: ...)`, with
  the in-flight test as a suspect only.
- **Without the isolation shim** the sweep drops `--no-fail-fast`, warns, and
  stops at the first failing harness; a complete profile refuses such a
  lane. The shim can't be installed under a configured runner or rustdoc, an
  ambient non-host build target, or a forwarded `--config` or `--target`. A
  `[[check]] target` sweep keeps the shim, which stands in for both the
  triple's runner and the host's.
- **A failing sweep ends the run** before later sweeps test, so their results
  are absent from a red run.
- **Warnings** from a passing sweep and its pre-builds are filtered by owner
  (a path dependency outside the workspace prints nothing) and printed once.

How harness isolation works, and the failure-roster rules, are in
`reference/check-internals.md`.

### Build errors

When a test sweep dies building (no `test result:` lines, compile errors on
stderr), brokkr condenses the errors one per line, as in the clippy phase. A
`linking with ... failed` error also keeps its undefined-symbol linker notes,
prefixed `linker:`. The one-line condensation would drop them, but they name
the actual cause. Both wild's `Undefined symbol X, referenced by` and GNU
ld's `undefined reference to` shapes surface.

When the undefined symbol is in the failing crate's own namespace and an
`.rcgu.o` (an incremental codegen unit) references it, that's the signature
of a stale incremental cache. The output then names the fix:
`stale incremental cache suspected ... run: brokkr clean --cargo PKG`. The
hint names the failing package, usually a workspace member, not the
`brokkr.toml` project name a bare `clean --cargo` defaults to. A genuinely
missing foreign symbol matches neither test, so ordinary link errors get the
notes but no hint. `brokkr test`'s BUILD FAILED reporter uses the same
detection and labels its summary line `cargo test:`. The support-build
reporter labels its line `cargo build:` over the same formatter and linker
detail.

A stale cache has a second form with no hint. Cargo's mtime-based
fingerprinting can fail to invalidate a dependency crate's rlib, as when a
source file is written with an mtime older than the cached fingerprint by a
tool that preserves timestamps. A `-p`-scoped test build then reports a
confident, false `E0599 no method named ...` against code that another shape
(or the clippy phase) compiles fine. Run `brokkr clean --cargo PKG` with the
stale dependency crate as `PKG`; touching any file in the stale crate also
forces the rebuild. No hint fires, because an E0599 is usually a real error
and brokkr can't tell the two apart.

### test_threads

By default the test phase runs each sweep serially (`--test-threads=1`)
under the per-test hang watchdog, attributing a stall from libtest's JSON
events. Parallel execution uses the same tracker and watchdog. A profile can
opt a plain libtest sweep into parallel execution with `test_threads`. A
plain sweep is an entry with no `parallel` key, under a profile without
process isolation (which takes only unset or 1):

- **unset or `1`**: serial, per-test watchdog (the default)
- **`0`**: libtest's default parallelism (num_cpus)
- **`N` (2 or more)**: `--test-threads=N`

On an entry with `parallel = { budget = N }` (see "Parallel test binaries")
the budget alone decides concurrency, and brokkr ignores the profile's
`test_threads`. A profile with any `test_threads` that references such an
entry is a load error, whatever the value: `1` reads as a promise of serial
execution and any other count as a tuning, and the key controls neither
there. For `1`, add a non-parallel twin for the serial profile to reference:
a second `[[check]]` entry with the same build shape and no `parallel` key.
For another count, drop it, or keep it on a profile of its own over the plain
libtest entries it drives and compose the two with `lanes`.

A parallel sweep enforces the same 20s per-test cap as the serial path, by the
same means: every lane drives libtest's JSON event stream
(`--format json -Z unstable-options`, injected automatically, native on
nightly). Each `started` event arms the cap for that test and each `ok` or
`failed` disarms it. brokkr names a test that crosses 20s and kills its
process group. It reconstructs the events into human libtest text, so the
filtered output looks as it always does. Every lane reserves `--format` for
the runner's JSON protocol.

This lane is for large workspaces where a few wall-clock-heavy tests
(live, network, multi-second lifecycle) dominate serial execution and
parallelism hides them, without giving up per-test hang protection. The
per-test clock is wall-clock, so a test that is merely CPU-starved under heavy
`test_threads` load can trip it; keep multi-second tests off the parallel
lane. `brokkr test` always runs serially, whatever the profile's
`test_threads`.

A `certifies = "complete"` profile refuses this cargo-mediated parallelism at
config load, and `prepare` refuses the same lane as a backstop. Many tests
and harnesses share one stream with no harness isolation, so their executions
can't be attributed. Use `parallel = { budget = N }` there: it runs the
binaries concurrently, each on its own stream. Its worker count is bounded by
the in-flight budget, and queued binaries don't each reserve an OS thread.

### Process isolation

A profile may set `isolation = "process"` (see `brokkr man sweeps`). The lane
prebuilds the sweep, lists each binary's filtered tests, and then executes
each prebuilt binary once per selected test with
`--exact NAME --test-threads=1`. It reconstructs the same cargo launch
contract as the parallel lane (see "Parallel test binaries").
`--test-threads=1` alone serializes tests within one process per test binary
but doesn't isolate them. Tests that touch process-global state, such as a
global logger, need the fresh-process guarantee CI's nextest provides.

The unit is the (binary, test) pair, not the name. Each pair is one process
with its own 20s wall, a kill is named by the caller's selection, and a
package-qualified skip applies per pair. So a skipped copy never blocks an
unskipped one in another binary. The lane:

- refuses a configured target runner, as the parallel lane does
- runs every test even after failures, since the per-test failure list is the
  point
- stops when a budget is blown
- fails the sweep if any test failed or zero tests were enumerated
- marks its shape lines `process-isolated`

This lane alone keeps a roll-call: the pre-run plan line, one
`PASS NAME (SECS)` per test, and a `SKIP` line per `#[ignore]`d name in a lane
without `include_ignored`. The roll-call goes to the run log, not stdout,
since a passing run's reader pays for every line. The lane's tests fold into
the grouped test line with its ignored and pkg-skipped totals. On a terminal
the status line names the test in flight, which is what the printed roll-call
was for; this is the slowest-per-test lane in the run. A failing test still
prints `FAIL NAME` with its command and output as it happens.

The lane works without a `brokkr.toml`, in any Rust and git repo. When a
`brokkr.toml` is present its host config still applies (nidhogg's
`CARGO_TARGET_TMPDIR`, say); when absent, cwd is the project root.

### Doctests

The test phase doesn't run doctests by default, for cost. Rustdoc compiles
and runs them in a pass of its own, apart from the test build: each example
separately before edition 2024, and in 2024 every example it can't merge
(`compile_fail`, `standalone_crate` and the like). That can make them too slow
for the edit loop in a project with many examples; the gate checks them
instead. Each sweep's `cargo test` is scoped to `--tests` (lib, bins and
integration tests, no doctests). A sweep that already has an explicit target
selector is the exception, and its selector decides the targets: a profile's
`--test NAME`, or a `--test`, `--lib`, `--doc`, ... token after
`brokkr check --`. brokkr doesn't append `--tests` on top of one.

You can opt back in two ways:

- `[test] doctests = true` restores the full `cargo test` default, doctests
  included.
- `doc_only = true` on an entry makes a sweep that runs `cargo test --doc` and
  nothing else, whatever the `[test]` flag. That's how a `parallel` or
  nextest build's doctests run outside a complete claim; see `doc_only` in
  `brokkr man sweeps`, including its refusals.

A `certifies = "complete"` profile runs doctests whatever the flag says, once
per tested build. For each lane whose build no `doc_only` entry of the same
build carries, it derives a `cargo test --doc` lane of exactly that build
(`ENTRY (doctests)`). brokkr announces it at the top of the run and accounts
for it per cargo resolution in the doctest block. The serial lanes run no
doctests inline, so none runs twice or under a lane's `skip` or `only`. A
`[[quarantine]] category = "doctests"` entry suppresses the derived carriers,
and the run names what it suppressed. The rules (which lanes owe doctests,
how a library's `doctest = false` is honoured, the workspace-shaped carrier a
claim needs) are under Doctests in `parallel` in `brokkr man sweeps`.

With `[test] doctests = false`, a run outside a complete profile skips
doctests unless a `doc_only` entry or a forwarded `--doc` selects them, and
brokkr doesn't warn. A project that never runs a complete profile leaves its
doctests unchecked unless it sets the key or selects them explicitly.

No CLI switch overrides `[test] doctests`, and `--skip` can't stand in for
one. Doctests share libtest's filter namespace with unit tests, so skipping
them by pattern would also skip legitimate module tests. `brokkr test NAME`
runs the full `cargo test` default, so a deliberately named doctest still
runs; within a doc-only sweep it runs `--doc NAME`.

`test_exclude_packages` on a `doc_only` sweep is the supported way to keep a
package, such as a bin-only crate, out of the doctest lane. It holds under
`brokkr check -p`: the `-p` intersection drops the excluded package from that
sweep, and a sweep left with nothing is excluded from the test phase. So
`-p EXCLUDED` never becomes `cargo test --doc -p EXCLUDED`. This is a
contract, pinned by a test.

### Parallel test binaries

A `[[check]]` entry with `parallel = { budget = N }` runs its test binaries
concurrently rather than letting cargo run them one at a time, under a budget
of tests in flight across the sweep. It exists for the wall-clock floor.
Cargo runs binaries sequentially and `--test-threads` parallelizes only
within one. So a sweep can't finish faster than the sum, over binaries, of
each binary's slowest test, a floor `test_threads` can't move.

The sweep builds once, then fans out by executing the prebuilt test binaries
directly, never re-entering cargo. What runs is exactly what the prebuild
built, on every selection shape, `test_exclude_packages` included. brokkr
reproduces cargo's launch contract for each binary: cwd, loader path,
`[env]`, build-script env, `CARGO_PKG_*` and `CARGO_BIN_EXE_*`. It doesn't set
`OUT_DIR`, which cargo never gives a test. As a result:

- A configured target runner or ambient build target refuses the lane, and
  every other direct-execution path, `brokkr test` included, rather than
  being bypassed. A sweep's own `target` key is the supported way to run for
  another triple; see `brokkr man sweeps`.
- Forwarded cargo args only cargo can honour (`--no-run`, `--target`,
  `--config`, `--manifest-path`, `--target-dir`) are rejected.
- Cargo target selectors after `--` (`--test NAME`, `--lib`, `--bins`, ...)
  narrow which binaries the sweep plans, unioned with the sweep's own
  `tests` and `lib_tests`.
- The sweep's `exclude_tests` then drops its targets from the plan, after the
  build. This lane, the process-isolated lane and the nextest lane (as a
  filterset term) can do that, since a serial lane hands cargo its selection
  whole. An exclusion matching no integration-test target the lane built
  fails the lane, as a stale `tests` name fails cargo. The re-verification
  before the lane runs holds the same binaries out.
- On a sweep selecting exactly the whole workspace,
  `feature_unification = "auto"` still promotes the prebuild to workspace
  unification; an explicit `selected` is honoured.

Each binary claims a share of the budget proportional to its serial cost from
the last run (`.brokkr/parallel-timings.toml`). The first run of a tree
weights by test count, and a budget change needs two runs to measure. The
budget forms:

- **`parallel = {}`**: the physical cores in one last-level cache domain
  (`brokkr env`'s `l3 domain:` line)
- **`budget = "cores"`**: every physical core the process may use

Both are capped by cgroup v2 `cpu.max` quotas, and the plan line names which
policy produced the number. The allocation rules and their rationale are
under `parallel` in `brokkr man sweeps`.

The lane writes its plan and result to the run log:

```text
[run]     test fanout: 35 binaries, budget 24 in flight (claims 1-3)
[run]     test fanout: 4210 passed in 31.2s (35 binaries, built in 48.9s, slowest broadarrow-worker/ba-worker 28.7s)
```

On stdout its tests fold into the grouped test line, which names the slowest
parallel binary: the sweep's floor, and the one to split. When two or more
binaries each claim the whole budget (budget above 1), they couldn't overlap,
and that prints as a warning.

A passing binary prints nothing of its own. A failing one prints its label, a
copy-pasteable repro command and its output. brokkr reports failures after
the join in plan order, so concurrent failures never interleave. A binary that
a signal ended also prints the last 40 lines of its own stderr. No cargo wraps
the launch to name the crash, and whatever the binary wrote before it died,
such as an abort guard's message, is there.

The lane is mutually exclusive with `isolation = "process"`. Doctests can't
run on a parallel sweep: `[test] doctests = true` needs at least one
non-parallel entry (a load error otherwise). A complete profile runs the
sweep's doctests in a doctest carrier of its build (see "Doctests").

## nextest

A `[[check]]` entry selects the nextest lane with `harness = "nextest"`.
brokkr links cargo-nextest's engine (`nextest-runner`) as a fast
process-per-test executor for a brokkr-configured sweep. The engine is an
executor, not a policy source. The reason is speed, as for `parallel`:
process-per-test removes per-test serialization. That matters most for a
sweep that needs the `isolation = "process"` guarantee, which on the libtest
path is N sequential prebuilt-binary spawns and here runs concurrently.
Linking rather than shelling out means no per-host install step and typed
test lists with no JSON round-trip. The engine version is brokkr's
`Cargo.toml` pin, printed on the lane's header line.

Everything the lane runs and claims comes from `brokkr.toml`. The engine runs
under a config brokkr synthesizes each run (`.brokkr/nextest-synth.toml`
under the state dir): retries zero, no default-filter, no test groups, no
setup scripts. Its `slow-timeout` and `terminate-after` are set to brokkr's
own 20s per-test watchdog ceiling, so the hang kill the other lanes have
comes per process, for free. brokkr never opens the checkout's
`.config/nextest.toml` and never reads `NEXTEST_PROFILE`: a foreign config
must not get a vote in what a brokkr gate runs.

### Division of labour

brokkr and the engine split the work this way:

- **brokkr owns the compile shape.** The build is brokkr's own
  `cargo test --no-run` with the sweep's selection, features, unification
  pin, cargo profile, `[lints]` allows and env, streamed into nextest's
  binary list builder. So a nextest lane and a libtest lane with equal compile
  inputs share the target dir and dedupe in clippy. `harness` is execution
  policy, never part of the build shape. The build's artifact stream must end
  in a `build-finished` record, for the run as for the coverage listing; a
  stream without one fails the lane rather than yielding an empty binary set.
- **Cargo's target runner is honoured, and a failure to resolve it fails the
  lane.** The engine resolves `[target.TRIPLE].runner` from the cargo config
  chain. When that resolution errors, the lane stops with the error rather
  than falling back to no runner. A fallback would execute binaries directly
  where cargo would have wrapped them (qemu, wine, valgrind): a wrong run
  whose output reveals nothing.
- **brokkr owns the test env.** The sweep env the libtest lanes set on
  `cargo test` reaches every test process and listing run the engine starts:
  `[[check]] env`, a profile's `env`, `BROKKR_TEST_BIN_DIR`, and host-specific
  values like nidhogg's `CARGO_TARGET_TMPDIR`. The engine builds its own
  commands, so brokkr passes the pairs through the one route it applies to
  all of them: cargo's `[env]` table, via a generated config file
  (`.brokkr/nextest-sweep-env.toml` under the state dir, rewritten per run).
  Its entries are `force = true`, so a sweep value beats an inherited one, as
  on the libtest lanes.
- **brokkr owns the filters.** See "nextest filters".
- **brokkr owns concurrency.** The profile's `test_threads` maps onto the
  engine's in-flight count; unset or 0 is the engine's num-cpus default.
  There's no serial default here: process-per-test needs no
  watchdog-attribution trick, and the synthesized `terminate-after` kills a
  hung test by name at any concurrency.
- **The engine owns execution and rendering**: process-per-test scheduling
  and its reporter. brokkr adds the sweep bookends and decides pass or fail
  from the run's outcome. brokkr records every engine event (a start, a
  finish, a retry, the cancellation) for the execution accounting before the
  reporter sees it, so a failing reporter can't take the record with it. A
  death the lane's own fail-fast caused (a signal after the cancellation
  began) is recorded as `interrupted`, not as the test's failure. An
  exit-code failure stays the test's own. Any engine timeout is `timed_out`,
  whatever the engine config would have made of it.
- **One listing, kept.** The lane executes the test list its preparation
  built. A second listing at run time could disagree with the plan about what
  the lane selects.

### Filters

brokkr translates the sweep's filters for the engine:

- The sweep's `only` and unqualified `skip` entries use nextest's own libtest
  pattern emulation, with identical substring semantics and nothing to
  escape.
- `include_ignored` maps to nextest's run-ignored policy.
- A package-qualified skip compiles to the term
  `not (package(P) & test(~X))`.

All of a sweep's qualified skips form one filterset, the conjunction of their
terms. The engine ORs the filtersets it's handed (a test is admitted if any
admits it), so one filterset per skip would make disjoint skips admit
everything. The translation uses `~` substring matchers because `test(=X)`
matches the full test path: an exact-match translation of a bare name would
silently match nothing. Filterset interpolation is restricted to
identifier-like characters and refuses anything else, because a wrongly
escaped filter silently changes which tests run.

### Refusals

The lane refuses:

- trailing `-- -- LIBTEST-ARGS`, since the engine takes no raw libtest argv;
  use the sweep's filters
- the forwarded cargo args the parallel lane rejects
- a `--format`-style libtest arg smuggled in through a profile
- filterset values outside the interpolation charset

These config compositions are load errors:

- `harness = "nextest"` with `parallel`, since that's two execution owners
- `harness = "nextest"` with `feature_unification = "package"`, which has no
  per-package plan
- `harness = "nextest"` on a profile that sets `isolation = "process"`. The
  engine already runs every test in its own process, and the intent should
  have one spelling.
- `[test] doctests = true` with no plain libtest entry, since nextest never
  executes doctests

Double-spawn is disabled (`NEXTEST_DOUBLE_SPAWN=0` semantics): nextest's
double-spawn protocol re-invokes the current executable, which is brokkr, not
cargo-nextest.

The in-process libtest lanes stay the default everywhere else, and this lane
doesn't replace them. A shared-process lane is the only detector for the
process-global-state class of bug (two tests contending over a global
logger). Process-per-test removes that contention and the bug's visibility
with it, so moving a detector sweep onto the engine retires the detector. Use
the engine where isolation or per-test fan-out is the point, not as a blanket
replacement.

### Coverage audit

The coverage audit covers nextest lanes, so a `certifies = "complete"`
profile may reference them. The exclusion model is the existing ledger,
unchanged, because the synthesized config has no default-filter and narrowing
comes only from `brokkr.toml` skips and quarantines. The pieces fit as
follows, measured against the linked `nextest-runner` version in
`Cargo.lock`:

- **A nextest lane's selection comes from the engine's own listing** under
  the sweep's real filters: the same test list the lane executes, so plan and
  execution can't disagree about what a sweep selects. `Selected` is the only
  executing verdict. Pattern mismatches (`skip`, `only`) report `string` and
  filterset mismatches (qualified skips) report `expression`, both the
  ordinary needs-a-lane-or-a-quarantine verdict. A verdict the ledger has no
  policy for refuses the lane's preparation, `default-filter` included; under
  the synthesized config it can't occur, so its appearance means a foreign
  config got a vote. brokkr maps the engine's binary ids onto its binary
  units through the lane's own artifact index, and an id the index doesn't
  hold refuses the lane too.
- **Pairs are per binary on every lane**, nextest or not (see "coverage").
  Two binaries in one package can define the same test path (measured: seven
  such collisions inside nautilus-infrastructure, all `serial_tests::`
  members), and the binary unit tells them apart. Package-qualified
  `[[quarantine]]` and skip entries mean every binary of package X.
- **The engine's mismatch reasons are priority-ordered, not a partition.** A
  test that is both `#[ignore]`d and unmatched by a lane's filterset reports
  `ignored`, so a single listing can't detect it as an orphan. brokkr takes
  one listing and inherits that precedence. A second `--run-ignored all`
  listing would detect only a test that is both ignored and uncovered, which
  needs neither covering nor quarantining. When you remove the `#[ignore]`,
  the verdict flips, the pair orphans, and the gate fails.
- **The universe comes from the shape's own per-binary enumeration**, never
  from a lane's listing. A package-scoped filterset reports the excluded
  package's suite as `skipped` with an empty testcase map, which would
  silently shrink the universe. brokkr reads lane listings only for their
  selections.

## coverage

Every run that runs tests prepares the whole invocation before its first test
(the execution inventory; see "Diagnostic continuation"). Under
`certifies = "complete"`, `prepare` brackets the test phase before it and
also plans the policy universe, and `coverage` follows it and reconciles the
plan against what was observed. Two claims come out, kept apart because they
are different claims:

- **Policy coverage**: every (shape, test) pair the profile could run is
  selected by some lane, or legitimately excluded (ignored, quarantined,
  curated). It's a property of the configuration, read from the plan.
- **Execution accounting**: every execution the plan expects ends as exactly
  one outcome (`passed`, `failed`, `timed_out`, `interrupted`, `ignored`,
  `unobserved`), taken from the typed observations the runners stream.

A timeout is an accounted failure, never coverage passed. `interrupted` and
`unobserved` are unresolved. A record that is complete after a timeout
certifies only that the failed run's record is complete, never the gate.

### prepare

Before any test runs, brokkr builds every attempted lane (support builds
first, then its shape, `cargo test --no-run`) and lists every binary with
nothing compiling alongside. It writes the plan to
`.brokkr/accounting/RUN-ID/plan.json`: the selected executions per lane, each
test executable's content hash, and the support executables. A journal beside
it records every observation as it happens, so it survives a watchdog kill.
brokkr keeps only the newest few run records.

A lane that runs its binaries directly (parallel, isolated, nextest) re-hashes
them before it runs. If they still match, it runs them with no cargo call. If
they changed, brokkr rebuilds the lane and checks it against the plan. Under a
complete claim any difference is a hard error ("artifacts changed since the
plan"), never a silent re-plan.

Every one of these checks reads the files in full. On a project with large
debug test executables, preparing costs read time proportional to their total
size several times over. The run log names what each hash site read in the
prepare phase and in the test phase
(`prepare: hashed N files at SITE, BYTES read in TIME`). Restore builds,
logged as `prepare: sweep 'LABEL': rebuilt before listing`, add more. They
come from sweeps that share one target dir and overwrite each other's
uplifted binaries. Folding disjoint feature sweeps into one package-mode
entry gives them a directory of their own (see `brokkr man sweeps`).

A serial or doc-only lane makes no build of its own before it runs. The lane
enters cargo once, for its `cargo test`, and that run's own build is what
gets checked. Its `build_packages` support builds run again first only when a
file the plan listed changed. When cargo reports the build finished, before
it starts any test, brokkr holds what it built to the plan, and the harness
isolation holds every test binary until that's done. Under a complete claim a
difference refuses that run's binaries before any test runs. Otherwise the
lane runs, and its inventory is reported unavailable with the reason. A
serial lane without harness isolation has no inventory to hold it to.

The run log names any test binary a lane listed with no tests at all, when
the lane neither filters by name nor selects targets explicitly. Every check
builds and spawns it for nothing; `test = false` on that target in
`Cargo.toml` skips both.

Under a complete claim, a lane that can't be prepared leaves the plan
incomplete and no tests run (`failed_phase: "prepare"`). Two lane shapes can't
be attributed and are refused there:

- a serial lane whose harness isolation can't be installed
- cargo-mediated parallelism (`test_threads` other than 1); use
  `parallel = { budget = N }` instead

### Green invariant

A complete run is green only when all of these hold:

- the plan is complete
- every expected execution `passed`
- nothing anomalous was seen: duplicate or unplanned records, totals that
  disagree, a suite left open
- no termination is on the record
- every planned doctest carrier completed

`ignored` and `interrupted` both fail it. Exit codes are unchanged: a fired
ceiling exits 124, an interrupt 130.

The identities (binary unit, pair, execution), the build, inventory and policy
passes, the outcome evidence table, the anomaly list and the doctest carrier
rules are in `reference/check-internals.md`.

### Policy coverage

Classification reads the plan: a shape resolution's selection is the union of
its lanes' executions. So a lane the fail-fast never reached still selects its
pairs, and they aren't orphaned; its executions are `unobserved` in the
accounting.

An incomplete plan reports what it knows, with status `incomplete`, never a
pass: the orphans of shapes whose every lane was prepared, and the dead
filters whose every sweep was listed. Stale quarantine entries aren't judged
there, since an entry matching nothing may match in a shape that couldn't be
prepared. `curated = true` entries are one declared narrowing of the
universe, and package-level `test_exclude_packages` is the other.

The universe is every `[[check]]` entry, not the profile's own sweep list.
Otherwise dropping a sweep from a lane would silently shrink the certified
set. So a `complete` profile that leaves a `[[check]]` entry referenced by no
sweep or lane is a load error: the entry would be enumerated nowhere, and the
audit would print `0 orphaned` over tests that never ran.

The exception is an entry that declares `curated = true` (see
`brokkr man sweeps`). A curated entry's non-selected pairs are outside the
universe by declaration, so leaving it unreferenced certifies nothing without
running, and the entry may live in its own deliberately run profile. When a
gate lane does run a curated entry, its build shape's non-selected pairs are
exempted rather than audited: counted and trailer-reported like
`test_exclude_packages`, never orphaned, never credited to a `[[quarantine]]`
entry. The exemption is keyed on the sweeps, not the shape, so a non-curated
entry sharing a build shape with a curated one keeps that shape fully
audited.

The universe and each lane's selection come from the binaries' own libtest
listings, under the lane's real filters, so libtest itself decides what a
filter admits. A shape's universe is its `--tests` set plus every target a
lane of the shape selects explicitly (`tests`, `lib_tests`), so a
`test = false` target a lane runs is audited like any other.

Every non-selected pair must be one of:

- **quarantined**: a `[[quarantine]]` pattern matches it, optionally
  package-scoped (every binary of that package), counted per entry. The most
  specific matching pattern takes the pair, so a narrow entry is never starved
  by a broad one it nests under.
- **ignored** at the source: counted and reported, not fatal.

Anything else is orphaned and fails the check, listed in full as
`shape/binary/test`. A pattern entry justifying zero pairs is stale and fails
the check. A run with stale entries, orphans and dead filters prints all the
worksheets before failing. Package-level `test_exclude_packages` is outside
the pair audit, since those binaries can't build, and the trailer calls it
out.

The ledger reports as a per-issue pair breakdown in descending order. On a
passing audit it appears in the coverage line's `quarantined` field
(`106 quarantined (B51 80, B41 14, B50 10, ...)`), whose count is the same
pair total. On a failing or incomplete audit, where that line doesn't print,
it's one rolled-up line of its own ahead of the findings
(`quarantine: 21 entries, 106 pairs - B51 80, B41 14, B50 10, ...`). That
keeps the countdown and the growth warning when a substring starts matching
more than it used to. It's a summary, not a cap: the line counts every pair
the ledger holds, at the granularity you act on (an issue, not the pattern
under it).

### Summary objects

The `--json` summary (schema 2) has the two claims as separate objects, plus
the doctest block and the run's termination:

```json
{
  "schema": 2,
  "verdict": "failed",
  "termination": { "kind": "per_test_timeout", "scope": "serial" },
  "policy_coverage": {
    "status": "passed", "plan_complete": true,
    "pairs": 0, "selected": 0, "ignored": 0, "quarantined": 0,
    "curated": 0, "orphaned": 0, "dead_filters": 0
  },
  "execution_accounting": {
    "scope": "binary_tests", "status": "incomplete",
    "expected_executions": 0, "passed": 0, "failed": 0, "timed_out": 0,
    "interrupted": 0, "ignored": 0, "unobserved": 0, "anomalies": 0
  },
  "doctests": { "inventory": "unavailable", "accounting": "unknown",
    "observed": { "passed": 0, "failed": 0, "ignored": 0,
      "carriers": [ { "lane": "doc", "completed": true, "streams": 1 } ] } }
}
```

A doctest carrier entry also lists two fields when non-empty:

- **`missing`**: the cargo resolutions it owed a rustdoc stream that
  presented none (a package name, or `the lane's selection` for a combined
  one)
- **`declined`**: the libraries a derived carrier declined for their
  manifest's `doctest = false`

`policy_coverage.status` is `passed`, `failed` (orphans, stale entries or
dead filters) or `incomplete`. `dead_filters` counts the dead `skip` and
`only` filters (see "Dead filters"). It's a field of its own because a dead
filter moves no pair between the other buckets. The execution counts
partition the expected executions, and `anomalies` is counted beside them.

All three blocks are present whenever the `prepare` phase ran, on a failed
run too, so a consumer of a failed gate sees the worksheet's numbers, not
`null`. They're `null` outside a complete profile, and on a complete-profile
run that failed before `prepare` (a gremlin, a clippy error, a pre-test
script check). No plan exists there, and a block of zeros would read as an
empty plan rather than no plan. `execution_accounting` isn't populated for a
run that certifies nothing, though that run has an inventory too: its scope
there would need stating on every field, and
`diagnostic_continuation.inventory` states it once.

`termination` is how the run stopped, as `{kind, scope}`, and `null` when
nothing stopped it. It's present outside complete profiles too. The kinds are
`phase_deadline` (scope: the phase), `interrupt`, `per_test_timeout`,
`run_deadline`, `sibling_timeout`, `fail_fast` and `engine_error`. Where the
stop was a lane's, the scope is the lane it covered.

When a lane's tests or record fail, the fail-fast it causes is scoped to the
first later lane it stopped, never one the selection excluded. Failing that
way in the last attempted lane stops nothing, so `termination` is `null`
unless the lane recorded its own (a timeout, say), and the error output names
the failed sweep. An operational error that ends the phase (a refusal, a
failed pre-build) is the run's, with scope `run`.

## Diagnostic continuation

After a watchdog kill, a per-test timeout, an interrupt or a fail-fast,
`check` and `brokkr test` account for every test the run left without a
verdict. They name each one, except plain fail-fast casualties, which are
counted. The report is a diagnostic: it reruns nothing, and a failed run never
turns green because of it. The motivating case was a downstream that merged
its integration tests into one binary per crate, so one hang in a serial lane
under a partial profile (the bare `brokkr check`) hid a whole crate's results.

This works because every run that runs tests, partial profiles and
`brokkr test` included, prepares an execution inventory up front (see
"coverage prepare"), without the policy universe a complete claim adds. The
inventory has three degraded states:

- **unavailable**: a lane that can't be attributed to binaries
  (cargo-mediated parallelism, no harness isolation). Only the suspects its
  shared stream blamed can be named.
- **unenumerable**: a binary that answers no libtest `--list`.
- **partial**: the inventory of a lane holding an unenumerable binary.

Outside a complete claim, brokkr records preparation failures and the lane
runs as it always does. A `harness = false` target in a partial profile's
serial lane is never executed to ask for a listing. The inventory rules,
preparation costs and `brokkr test`'s per-sweep recheck are in
`reference/check-internals.md`.

### Report

On the final reporting path, after the failure verdict and before the `--json`
trailer, the header counts every unresolved execution: `interrupted` or
`unobserved`. Failed, timed-out, passed and ignored executions stand as the
results they are. Every one that a kill, a hang, a deadline or an interrupt
left is named, with no cap, grouped by lane, resolution and binary, with its
outcome and detail:

```text
diagnostic continuation: 3 unresolved executions (of 3 expected)

default / crate-a / test:integration
  interrupted  detector::alpha  phase deadline
  unobserved   detector::beta   phase deadline
  unobserved   detector::gamma  phase deadline

note: diagnostic only: ...
note: an unobserved execution may have run if records were lost; ...
```

The exception is the plain fail-fast casualty: an execution that never
started only because an earlier failure stopped the run (`unobserved`, detail
`fail_fast`). The text prints one line per binary with a count instead of the
names:

- `default / crate-a / test:later: not reached, 40 tests` for a binary that
  never started
- `  not reached: 40 tests` under the heading of a binary that did start or
  also has named executions

For a hang each unresolved test is a suspect, which is why the rest are
named. For an ordinary assertion failure the list would be almost entirely
fail-fast casualties burying the failure that caused them. The full names stay
in the `--json` `candidates`, and a fail-fast victim that was interrupted
(started, then cut down) is still named.

A binary of which nothing was observed and that has named executions is
marked `(not reached)`. A lane whose inventory is unavailable or partial says
so under the list. The text ends with the statement (diagnostic only,
certifies nothing, never changes the verdict) and the evidence caveat; the
statement also stays in the `--json` trailer's `statement`. On a failed run
whose continuation prints, the `accounting:` counts line is log-only, since
the header has the unresolved and expected counts. Anomalies and journal
problems still print.

`--json` (schema 2, additive) has `diagnostic_continuation`. It's `null` on a
run that passed, left nothing unresolved, or failed before `prepare`:

```json
"diagnostic_continuation": {
  "source_run_id": "123456-2",
  "certifies": false,
  "statement": "diagnostic only: ...",
  "inventory": {
    "scope": "binary_tests",
    "availability": "complete | partial | unavailable",
    "plan_complete": true, "journal_closed": false,
    "expected_executions": 0, "unresolved": 3, "anomalies": 0,
    "lanes": [ { "lane": 0, "label": "default", "kind": "serial",
      "availability":
        "complete | partial | unavailable | skipped | doctests",
      "reason": null, "expected_executions": 0 } ],
    "suspects": []
  },
  "candidates": [ { "lane": "default", "resolution": null,
    "package": "crate-a", "binary": "test:integration",
    "test": "detector::alpha", "outcome": "interrupted",
    "detail": "phase_deadline",
    "execution": { "lane": 0, "attempt": 1, "pair": { } } } ]
}
```

`candidates[].execution.pair.shape` is an opaque declared-shape digest. Its
versioned encoding can change the digest value between brokkr versions; the
schema and field stay the same, and the digest isn't a cross-run key.

`policy_coverage` stays `null` for a run that certifies nothing, and
`execution_accounting` isn't populated there either (see "coverage summary
objects"): the inventory's scope is stated once, here.

The outcome words mean this, and no more:

- **`timed_out`**: the execution exceeded its attributed budget. That doesn't
  prove it caused a hang.
- **`interrupted`**: a start was observed without an acceptable terminal.
- **`unobserved`**: no start and no terminal were recorded. It may have run
  if records were lost; missing evidence isn't proof of non-execution.

A harness that passes every test and then exits nonzero, or dies by a signal,
with nothing else to explain it is an `unclean_exit` anomaly, so the run isn't
green. A nonzero exit that a failed test, a kill or a stop explains isn't
counted twice.

## Feature unification

A `[[check]]` entry's `feature_unification` chooses which feature graph its
cargo invocations resolve against:

| Value | Meaning |
|---|---|
| `auto` (default) | brokkr policy: pin nothing, except the parallel lane's whole-workspace promotion (see "test parallel test binaries"). |
| `selected` | Pin cargo's default: each selected package resolved with only the features that selection requires. |
| `workspace` | Pin workspace-wide unification: every member contributes features, whatever the selection. |
| `package` | Pin per-package resolution, the shape `cargo install` produces. Requires a non-empty `packages`. |

The three explicit values mirror cargo's own `resolver.feature-unification`
vocabulary. Each pins its value on the argv (`-Zfeature-unification` plus a
`--config`), so a lane's resolution doesn't depend on whether the consuming
repository has a matching `[resolver]` block in `.cargo/config.toml`. A CLI
`--config` outranks a config file, so the pin wins. `auto` is the one value
that defers to ambient config, which is why the default is spelled `auto`
rather than `selected`: absence and `auto` mean the same thing, so writing
the default out can't change behaviour.

### Install shape

The member_check phase compiles the install shape but runs no tests, so it
can't see a runtime defect: a crate whose behaviour depends on a feature a
sibling workspace member enables. Take a daemon that depends on a crate with
`default-features = false` and calls a `#[cfg(feature)]`-gated path. It
compiles identically either way. Under workspace resolution a sibling donates
the feature and every dev build passes, while every installed binary takes
the other arm. Only running that package's tests in package resolution tells
them apart. These two entries run two such lanes:

```toml
[[check]]
name = "daemon-install-shape"
packages = ["broadarrow-daemon"]
feature_unification = "package"

[[check]]
name = "run-prep-adapter-free"
packages = ["broadarrow-run-prep"]
feature_unification = "package"
no_default_features = true
```

The second lane is why this is an explicit key rather than a flag derived from
`[bin] install`. A crate's install shape and its adapter-free consumer shape
are different questions, and only the first could be derived.

### Package mode

`feature_unification = "package"` has these consequences:

- **One cargo invocation per package**, never a batched multi-`-p`, for the
  same measured reason as the member_check phase (see "member_check"). This
  applies to clippy, the prebuild, the test run, the parallel fan-out and the
  `prepare` phase's enumeration alike.
- **Its own target dir** (`TARGET/unify-package[-RUSTFLAGS-HASH]`). A
  different resolution re-fingerprints units and would otherwise rebuild
  against every ordinary sweep on every run, in both directions. The lane's
  per-package resolutions share the dir: cargo fingerprints independent
  invocations correctly within one directory. A lane exports a single
  `BROKKR_TEST_BIN_DIR`, so a directory per package would leave a
  multi-package lane with no one place holding the binaries its tests spawn.
- **Its own coverage shape per resolution.** Under `certifies = "complete"` a
  two-package package-mode lane audits as two shape resolutions, labelled
  `ENTRY/PACKAGE`, its pairs tagged with the resolution. It doesn't need
  `curated = true`: package scoping narrows the universe and the selection
  together, so an unfiltered lane selects its whole universe and leaves no
  orphans.
- **Forwarded package selectors are refused.** `brokkr check -- -p other`
  under package mode is an error, because cargo unions selection flags and
  would widen a resolution promised to hold exactly one package.
- **Each resolution gets only its package's features.** brokkr projects an
  entry's package-qualified tokens onto each resolution's one package, so
  `strategy/hotpath` doesn't reach the `vm` resolution (see
  `brokkr man check feature-projection`).
- **Each resolution gets only its package's target selectors.** A `--lib` or
  `--test NAME` reaches a resolution's prebuild only when its package has
  that target, since cargo refuses one it lacks. So an explicitly selected
  `test = false` target is built there as everywhere else. A package with
  none of the selected targets builds its default set, which the selectors
  then narrow to nothing.
- **The mode is part of the build shape**, and of what `brokkr test` dedupes
  on. A package-mode and a workspace-mode lane never dedupe into each other in
  clippy, in `brokkr test`, or in the coverage audit.

## member_check

The member_check phase resolves each workspace member alone. Every other
compiling phase resolves features over a selection that includes sibling
workspace members, so a shared dependency's features are the union of what
all members ask for. `cargo install -p MEMBER`, and `brokkr install` per
`[bin] install` package, resolves each package alone. A call site that
compiles only because a sibling enables a third-party feature (a tokio,
hyper or upstream-crate feature, not only the workspace's own) is green under
every workspace-selected sweep and fails at install time, on the deploy path.

The phase closes that gap with one `cargo check` per workspace member under
`-Zfeature-unification` with `resolver.feature-unification="package"`. That
cargo mode resolves a selected package as though it were selected alone,
which is `cargo install`'s boundary. The invocation also passes `--locked`,
`--keep-going`, the configured profile, and the `[lints]` allows through the
same rustflags plumbing as the test phase. The CLI `--config` outranks a
repo's committed `.cargo/config.toml`, so a workspace-mode unification pin
(adopted for the parallel lane) and this phase coexist.

brokkr checks every member even after an earlier one fails, so one run
reports the whole workspace. The member builds and their `--bin NAME`
eligibility probes all use the `unify-package` target directory beside
Cargo's metadata-resolved target dir. That keeps this package graph out of
the workspace-shared cache while every member and every run reuse it
incrementally.

The `[member_check]` table configures the phase; this one shows each key:

```toml
[member_check]
when = "gate"               # "off" | "gate" (default) | "always"
targets = ["lib", "bins"]   # default; any non-empty subset
profile = "dev"             # "dev" (default) | "release"
skip = [{ package = "xtask", reason = "tooling, never installed" }]
```

The keys mean:

- **Members** are cargo metadata's `workspace_members`, so `exclude` is
  respected, less `skip`. Every `skip` entry has a reason. One naming no
  member fails the phase, validated against the whole member set before any
  `-p` narrowing, so a narrowed run can't hide a stale entry. There's
  deliberately no allowlist: a member added later is checked, not silently
  left out.
- **`targets`** spells `lib` and `bins`, nothing else: production targets
  only. Tests, benches and examples pull the member's dev-dependencies into
  the same resolve, and a dev-dependency can donate a feature to a dependency
  the production targets also use. Package unification isolates members from
  each other, not a member's targets from each other. A member with no target
  of one selected class is fine as long as it has one of the other; a member
  with none at all fails (skip it with a reason).
- **`profile`** is part of the claim and named on the ok line. Code can
  differ under `debug_assertions`, so a dev check says nothing about release.

The phase invokes cargo per package rather than once with a batched
multi-`-p`, because of a measured false positive. Cargo dedupes units across a
batched graph. With two feature variants of a shared dependency in the batch,
a path dependency whose own features don't vary dedupes onto one variant,
while a crate between them compiles against both. That produced
"expected `AccountId`, found `AccountId`" errors in an innocent intermediate
crate, a shape no real `cargo install` can resolve into. Per-package
invocations are the advertised boundary, and they attribute every error to
the member whose resolve produced it.

When one resolve builds more than one feature set of a crate version, the
failure output names the crate as a candidate cause. Otherwise rustc's
"mismatched types" between identical type names reads as version skew that
isn't there. It's only a candidate: host and target dependency domains may
legitimately differ.

CLI `-p` intersects with the member set. Named members are checked, a skipped
or non-member name is dropped with a note, and an emptied intersection skips
the phase visibly. `--gate` refuses `-p` outright.

### What green claims

A green run claims exactly this: each checked member's selected production
targets compile under that member's own resolve, at the named profile,
against the workspace lockfile. It doesn't claim the member will
`cargo install`, since the check performs no codegen or linking. It shares a
dependency graph with `brokkr install`, which passes `--locked` (see
`brokkr man run`). Raw `cargo install --path` and `brokkr install --unlocked`
re-resolve instead, and then this phase's green says nothing about the
versions they pick.

Two refusals keep it honest:

- **Zero-target refusal.** `--bins` silently skips a bin whose
  `required-features` are unsatisfied, so brokkr compares the artifact stream
  against each member's declared production targets. It keys them by the
  member's exact package id, target class and name, and credits only non-test
  compiles, so a gate that checked nothing can't read as green. A lib missing
  from a successful stream fails, since a lib has no `required-features` to
  excuse it. A bin missing from the stream is put to cargo by name
  (`--bin NAME`, every other resolution argument identical). A build whose
  stream has the bin's artifact credits it, a compile error fails the member,
  and cargo's own `requires the features` refusal naming that target waives
  it, since `cargo install` would skip it too. Anything else fails closed.
  Waived bins are reported by name, and a member whose targets are all waived
  still fails.
- **No toolchain fallback.** A cargo without package-mode support fails with
  a named remedy: update nightly, or turn the phase off
  (`[member_check] when = "off"`, or `[bin] install_feature_check = "off"`
  for the install gate). Falling back to selected mode would silently
  recreate the workspace-unified graph the phase exists to leave. That
  classification aborts the phase, so it requires a positive match on cargo's
  own option rejection and the absence of rustc diagnostics. A substring test
  for "nightly" would match ordinary compile output and a dependency named
  `nightly-helper` alike, and abort the remaining members with a false
  remedy.

brokkr deliberately doesn't decide bin eligibility from `required-features`
itself. That would mean reproducing namespaced features, `dep:` suppression
of implicit features, weak dependency features and `dep/feat` requirements. A
model wrong in the permissive direction drops a bin from the expectations,
never compiles it, and reports green: cargo's silent skip moved inside
brokkr, where nothing is left to catch it.

### Failure output

On failure the errors are ordinary rustc errors against code that compiles
fine in the rest of the run, which is confusing without context. So the phase
offers a candidate explanation. It diffs the workspace resolve against the
failing member's solo resolve and prints which features each dependency
loses ("hyper v1.11.0: server"). The solo resolve is `cargo tree`'s
normal and build graph with default features, an approximation of the
failing build labelled as one. When one of those features is what the code
needs, declare it in the member's own manifest. The errors print in the
one-line form like every other phase. For a low-noise debug loop, run with
`-p MEMBER`, which narrows the test phases and the member set together.

### when

`when` takes `off`, `gate` or `always`, default `gate`: the phase runs only
under a `certifies = "complete"` profile. Per-member resolution deliberately
compiles duplicate variants of shared dependencies, so it's priced for the
pre-landing run, not the edit loop. `[[script_check]]` takes the same key but
defaults to `always`.

Without a `[member_check]` table, a non-empty `[bin] install` implies an
install gate. The phase runs over the install packages only, their bins only,
at the profile `brokkr install` builds (`[bin] debug`), at
`[bin] install_feature_check`'s cadence (default `gate`). A package there that
isn't a workspace member fails the phase. A `[member_check]` table supersedes
the implied gate with every member and its own targets, profile and `when`, so
`install_feature_check` beside one is a load error. With neither, the phase is
inert.

A partial profile may name `member_check` in `skip_phases`; `install_feature`
is accepted there as the same phase. The phase budget is the test phase's,
since it covers a whole workspace of cold resolves; the whole-check ceiling
still bounds the run.

## Dead filters

The staleness rule the ledger applies to `[[quarantine]]` entries applies to
`skip` and `only` filters too: a filter that matches no test is a defect, not
a no-op. During the `prepare` phase brokkr asserts every `skip` and every
`only` declared on a `[test.profiles.*]` block or a `[[check]]` entry against
the enumeration. Under a `certifies = "complete"` profile, one that matched
nothing fails the check in the `coverage` phase, named with the block it was
written in:

```text
[error]   dead filter: only "read_market_latency" in [test.profiles.timing] - matches no test in any sweep it applies to (timing)
```

Any other run judges it from narrower evidence and fails too (see "dead
filters partial runs").

A dead `skip` is a name that drifted. Whatever it excluded runs again under a
name nobody wrote down, or it will start catching an unrelated test that
grows into the substring later. A dead `only` is worse, because the lane then
evaluates nothing. A sweep declared to enforce a wall-clock contract, whose
filter no longer matches, is a gate that has stopped existing while the
config still shows it as checked. The orphan audit sees neither: a dead skip
subtracts nothing from the lane's claim, so no pair goes non-run and nothing
is orphaned.

### Matching rules

These rules decide what "matched nothing" means:

- **Judged against the lane's binaries, not the shape's universe.** A lane
  narrows binaries with `tests = [...]` (`--test TARGET`), `lib_tests` and
  `exclude_tests`, so brokkr matches a filter against the names that lane can
  see. An `only` that matches nothing outside an excluded target is dead.
  Against the wider universe a skip would read as alive on the strength of a
  match inside a binary the lane narrowed away.
- **The reference set is the filter's own scope**: the union of the
  candidate sets of the sweeps it applies to. A `[[check]]` filter is judged
  against the lanes running that entry, and a `[test.profiles.*]` filter
  against every sweep the profile runs. The claims differ ("this test should
  not run in this sweep" versus "...in this profile"), and the profile claim
  is satisfied by a match anywhere the profile runs. Judging a profile filter
  per sweep would report a false death for nearly every entry of a profile
  that combines an unscoped sweep with a package-scoped one. Each skip names
  a test outside the scoped sweep's packages, so it's dead there while doing
  its job in the unscoped one. A filter dead in every sweep it applies to
  still reports.
- **`skip` and `only` run against different sets.** A `skip` is dead when
  nothing it could remove exists. An `only` is dead when the lane evaluates
  nothing under it, so the skips and, on a lane without `include_ignored`,
  the `#[ignore]`d names come out first. Otherwise an `only` whose every match
  is skipped or ignored would count as matching while selecting no work.
- **Each filter is asserted individually.** libtest ORs positional filters,
  so a lane with one live `only` and one dead one still runs tests and looks
  healthy. Folding the assertion the way libtest folds the filters would let
  the live sibling cover for the dead one.
- **Package-qualified skips match within their package only**, as
  `package = "PKG"` scopes a `[[quarantine]]` entry.

brokkr judges every prepared lane, whether or not the test phase reached it:
a filter's liveness is a property of what the lane selects, which the plan
knows before anything runs. A filter one of whose sweeps couldn't be prepared
is unknown rather than dead, since the unlisted sweep may be where it
matches, and isn't reported.

Resolving filters against libtest's own enumeration is a correctness choice.
Deriving test names by parsing Rust source would have to learn every
declaration form (`#[tokio::test]`, `macro_rules!`-generated tests, and so
on), and a parser that has gone blind agrees with every filter list.

### Partial runs

The policy audit runs only under `certifies = "complete"`. Every other run
judges the same filters in `prepare`, by the same rules, and reports each dead
one once the test phase ends. A dead filter fails a green test phase in the
`coverage` phase, with the same message:

```text
[error]   dead filter: skip "sort_overlapping_blobs_uring" in [test.profiles.full] - matches no test in any sweep it applies to (all)
```

A failed test phase stays the run's verdict, and the dead filters print
beside it.

There's no policy universe there, so the evidence is each lane's own. A lane
whose sweep has a `skip` or `only` takes two additional listings of each of
its binaries, with no filters: the same two a universe takes. brokkr reports
a filter only where that evidence is sound. The filter is unknown, and
silent, when any sweep it applies to is:

- not attempted by this invocation (a CLI `-p` that leaves it out), couldn't
  be prepared, or has no inventory (a serial lane whose harnesses can't be
  attributed)
- a nextest lane, since a lane's engine listing is never existence evidence
  (a package-scoped filterset reports a whole suite as skipped with no
  testcases), or a `doc_only` lane
- a serial lane that also runs doctests, since its filters reach doctests
  nobody listed
- run with forwarded libtest args (`brokkr check -- -- --exact ...`): the
  verdict is about the configured filters under libtest's substring rule, and
  a forwarded `--exact`, `--ignored` or `--include-ignored` changes what they
  select
- holding a binary whose unfiltered listing failed or that can't be listed

### Run-time guard

A second guard covers the other direction. When a sweep runs and its filters
collect no work, the test phase refuses:

```text
cargo test: zero tests ran (sweep: timing) (1 suite, 99 filtered out)
  - a profile/filter combo collected no work; treat as a wrong-run.
```

So the run-time guard catches an `only` that leaves its sweep no work in any
sweep that runs, though a live sibling `only` hides a dead one from it. The
alive-check covers the case that's enumerated but not run: a curated entry
referenced by a complete profile, whose lane the audit enumerates. Between
them the only gap is a sweep neither enumerated under a complete profile nor
ever run. That's a lane nobody evaluates, a larger and different defect that
no filter check can fix.

The minimum length (see "dead filters minimum length") applies everywhere,
under every profile, because brokkr enforces it at config load rather than in
either phase.

### Minimum length

A `skip` or `only` substring shorter than four characters is a load error in
any `[[check]]` entry or `[test.profiles.*]` block. For a package-qualified
skip the floor is on the `pattern` half; the `package` half is an exact name:

```text
[[check]] entry 'unit' has `skip` filter "ser", shorter than 4 characters. ...
```

The floor closes the hazard the alive-check can't see. A very short substring
is a substring of nearly every test name, so it suppresses or selects tests
nobody chose while always matching something, and satisfies "matched at
least one test" vacuously. The alive-check catches a filter that matches too
little; the floor catches one that matches too much.

brokkr enforces it at load rather than in the `coverage` phase, because that
phase runs only under a complete profile and a degenerate filter is
degenerate wherever it's declared. Loading also knows the config location
exactly. Because the floor is a load error, a `brokkr.toml` with a
three-character filter doesn't load at all, and every command fails, not only
`check`. For a broad filter, write a longer substring; no opt-in flag exists.

## Test environment

Both `brokkr check` (test phase) and `brokkr test` set `BROKKR_TEST_BIN_DIR`
on every `cargo test` invocation, sweeps with empty `build_packages`
included. It names the directory holding the just-rebuilt `build_packages`
artefacts:

- **`brokkr check`** sets it to `TARGET/debug`, since the test phase runs
  without `--release`. A sweep with `profile = "release"` gets
  `TARGET/release` instead, and the whole sweep (clippy, pre-build, test run,
  coverage enumeration) compiles release.
- **`brokkr test`** sets `TARGET/release` by default, and `TARGET/debug` when
  `--debug` or `[test] debug` applies. It follows the sweep's own `profile`
  when the sweep declares one.

The profile tracks the cargo invocation one to one; it doesn't track whatever
profile cargo compiles the test harness with. `TARGET` comes from
`cargo metadata --no-deps`. A sweep with a `target` triple gets
`TARGET/TRIPLE/PROFILE`, where its own artifacts go. Host units cargo builds
alongside, such as a proc-macro's test harness, receive the same directory.

Tests that spawn the rebuilt binary should read this variable as the primary
source of truth, and fall back to `cfg!(debug_assertions)` only when it's
unset, as under plain `cargo test` outside brokkr. The `cfg!` heuristic is
unreliable because a `[profile.test]` override can set
`debug-assertions = false` in the test binary even though the rebuilt binary
lives under `debug/`.
