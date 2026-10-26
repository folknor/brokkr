# check: the test phase and its lanes

The test-running half of `brokkr check` (and the lanes `brokkr test` shares):
how sweeps run, what a complete profile audits, and what a failed run reports.
The command, its output and sweep selection are in `brokkr man check`; time
ceilings in `brokkr man hygiene`.

## `test` phase

Runs after clippy, one lane per selected sweep.

- **Lint suppression.** `[lints] allow` and `allow_exact` reach every compiling
  call site of the run (pre-builds, prebuilds, `prepare`'s builds), through
  cargo's rustflags. `allow_exact` loses its file scope here and applies
  build-wide, and only rustc's own lints are passed (`clippy::`, `rustdoc::`
  and `cargo::` names never reach a build). See `docs/brokkr.toml.md`
  (`[lints]`).
- **The failure list names every failure.** The serial lane passes
  `--no-fail-fast` and isolates each test harness on its own stream, so a
  harness that crashes cannot hide or misattribute the failures of the
  harnesses after it. A crash shows as `CRASHED <harness> (signal: ...)` with
  the in-flight test as a suspect only. Where the isolation shim cannot be
  installed (a configured runner or rustdoc, an ambient non-host build target,
  a forwarded `--config`/`--target`), the sweep drops `--no-fail-fast`, warns,
  and stops at the first failing harness; a complete profile refuses such a
  lane. A `[[check]] target` sweep keeps the shim: it stands in for both the
  triple's runner and the host's.
- A failing sweep ends the run before later sweeps test, so their results are
  absent from a red run.
- Warnings from a passing sweep and its pre-builds are owner-filtered (a path
  dependency outside the workspace prints nothing) and printed once.

How harness isolation works, and the failure-roster rules, are in
`reference/check-internals.md`.

### Build errors and the linker hint

When a test sweep dies building (no `test result:` lines, compile errors on
stderr), the errors are condensed one-per-line like the clippy phase. A
`linking with ... failed` error additionally keeps its undefined-symbol linker
notes (dropped by the one-line condensation, but they carry the actual cause -
wild's `Undefined symbol X, referenced by` and GNU ld's `undefined reference
to` shapes both surface, prefixed `linker:`). When the undefined symbol lives
in the failing crate's *own* namespace and is referenced by an `.rcgu.o`
(an incremental codegen unit), that's the stale-incremental phantom-symbol
signature, and the output names the fix directly:
`stale incremental cache suspected ... run: brokkr clean --cargo <pkg>` -
with the *failing* package, which is generally a workspace member, not the
brokkr.toml project name that a bare `clean --cargo` would default to. A
genuinely missing foreign symbol matches neither test, so ordinary link
errors get the notes but no hint. Detection is `linker_failure_detail` in
`src/cargo_filter.rs`, on the `filter_test` fallback path (shared with
`brokkr test`'s BUILD FAILED reporter via `filter_test_build_failure`, which
labels the summary line `cargo test:`; the support-build reporter uses
`filter_cargo_build_failure`, labelled `cargo build:`, over the same
formatter and linker detail).

The stale-incremental family has a second, hintless face: cargo's
mtime-based fingerprinting fails to invalidate a dependency crate's rlib
(seen when a source file lands with an mtime *older* than the cached
fingerprint - e.g. written by an external tool that preserves timestamps),
and a `-p`-scoped test build then reports a confident, false `E0599 no
method named ...` against code that a differently-scoped shape (or the
clippy phase) compiles fine. Same code, one shape green, one shape phantom
errors. The remedy is the same `brokkr clean --cargo <pkg>` (with the stale
*dependency* crate as `<pkg>`); touching any file in the stale crate also
forces the rebuild. No automatic hint fires here - an E0599 is usually a
real error, and brokkr can't tell the two apart.

### Serial vs parallel sweeps

By default the test phase runs each sweep serial (`--test-threads=1`) under
the per-test hang watchdog, attributing a stall from libtest's JSON events.
Parallel execution uses the same tracker and watchdog. A profile can opt a sweep into
parallel execution with `test_threads`:

- unset or `test_threads = 1` - serial, per-test watchdog (the default; nothing
  changes for existing projects).
- `test_threads = 0` - libtest's default parallelism (num_cpus).
- `test_threads = N` (>= 2) - `--test-threads=N`.

A parallel sweep enforces the same 20s per-test cap as the serial path, by the
same means: **every lane drives libtest's JSON event stream**
(`--format json -Z unstable-options`, injected automatically; native on nightly).
Each `started` event arms the cap for that test, each `ok`/`failed` disarms it,
and a test that crosses 20s is named and its process group killed. The events are
reconstructed back into human libtest text, so the filtered output looks
exactly as it always did.

This lane is for large workspaces where serial execution is
dominated by a few wall-clock-heavy tests (live/network/multi-second lifecycle)
that parallelism hides - now without surrendering per-test hang protection.
Because the per-test clock is wall-clock, a test that is merely CPU-starved
under heavy `test_threads` load (not hung) can trip it; keep genuinely
multi-second tests off the parallel lane. Every lane reserves `--format` for
the runner's JSON protocol. `brokkr test`
is unaffected - it is always serial regardless of the profile's `test_threads`.

A `certifies = "complete"` profile refuses this cargo-mediated parallelism
during `prepare`: many tests and harnesses share one stream with no harness
isolation, so its executions cannot be attributed. `parallel = { budget = N }`
(below) runs the binaries concurrently with each one on its own stream, and is
the lane to use there. Its worker count is bounded by the in-flight budget;
queued binaries do not each reserve an OS thread.

### Process isolation (`isolation = "process"`)

A profile may set `isolation = "process"` (see `docs/brokkr.toml.sweeps.md`): the
lane prebuilds the sweep, lists each binary's filtered tests, and then
**executes each prebuilt binary once per selected test** with
`--exact <name> --test-threads=1`, under the same reconstructed cargo launch
contract as the parallel lane (below). `--test-threads=1` alone serializes
tests within one process per test binary; it does not isolate them, and tests
touching process-global state (a global logger) need the fresh-process
guarantee CI's nextest provides.

The unit is the **(binary, test) pair**, not the name: each pair is one process
with its own 20s wall, the kill is named by the caller's selection, and a
package-qualified skip applies per pair, so a skipped copy never blocks an
unskipped one in another binary. A configured
target runner refuses the lane, as it does the parallel one. Every test runs
even after failures (the per-test failure list is the point); a blown budget
stops the lane; the sweep fails if any test failed or if zero tests were
enumerated. Shape lines carry `process-isolated`.

This lane - and only this lane - keeps a **roll-call**: the pre-run plan
line, one `PASS <name> (<secs>)` per test, and a `SKIP` line per `#[ignore]`d
name in a lane without `include_ignored`. It goes to the run log, not stdout:
a passing run's reader pays for every line, and the lane's tests fold into the
grouped test line with its ignored and pkg-skipped totals. It is the
slowest-per-test lane in the run, which is where progress is worth watching -
on a terminal the status line names the test in flight, which is what the
printed roll-call was for. A failing test still prints `FAIL <name>` with its
command and output as it happens.

Works without a `brokkr.toml` - usable in any Rust+git repo. When a
`brokkr.toml` is present its host config still applies (e.g. Nidhogg's
`CARGO_TARGET_TMPDIR`); when absent, cwd is the project root.

### Doctests

The test phase does **not** run doctests by default, for cost: rustdoc
compiles and runs them in a pass of its own, apart from the test build -
each example separately before edition 2024, and in 2024 every example it
cannot merge (`compile_fail`, `standalone_crate` and the like) - which can
make them too slow for the edit loop in a project with many examples. The gate
checks them instead (below). Each sweep's `cargo test` is scoped to `--tests`
(lib + bins + integration tests, no doctests) - **unless** the sweep already
carries an explicit target selector (a profile's `--test <name>`, or a
`--test`/`--lib`/`--doc`/... token after `brokkr check --`), which then
decides the targets itself; `--tests` is not appended on top of one.

Opt a project back in with `[test] doctests = true`, which restores the full
`cargo test` default (doctests included), or per entry with
`doc_only = true` - a sweep that runs `cargo test --doc` and nothing else,
regardless of the `[test]` flag (how a `parallel` or nextest build's doctests
run outside a complete claim; see `[[check]] doc_only` in
`docs/brokkr.toml.sweeps.md`, including its refusals).

**A `certifies = "complete"` profile runs doctests whatever the flag says**,
once per tested build: for each lane whose build no `doc_only` entry of the
same build carries, it derives a `cargo test --doc` lane of exactly that
build (`<entry> (doctests)`), announced at the top of the run and accounted
per cargo resolution in the doctest block. Its serial lanes run no doctests
inline, so none runs twice or under a lane's `skip`/`only`. A
`[[quarantine]] category = "doctests"` entry suppresses the derived carriers,
and the run names what it suppressed. The rules - which lanes owe doctests,
how a library's `doctest = false` is honoured, the workspace-shaped carrier a
claim needs - are under `parallel` -> Doctests in
`docs/brokkr.toml.sweeps.md`.

With `[test] doctests = false`, then, a run outside a complete profile skips
doctests unless a `doc_only` entry or a forwarded `--doc` selects them, and
brokkr does not warn about it: a project that never runs a complete profile
leaves its doctests unchecked unless it sets the key or selects them
explicitly.

No CLI switch overrides `[test] doctests`, and `--skip` cannot stand in for
one: doctests share libtest's filter namespace with unit tests, so skipping
them by pattern would eat legitimate module tests too. `brokkr test <name>` is
unaffected - it runs the full `cargo test` default so a deliberately named
doctest still runs (and within a doc-only sweep it runs `--doc <name>`).

`test_exclude_packages` on a `doc_only` sweep is the supported way to keep a
package out of the doctest lane (a bin-only crate, say), and it holds under
`brokkr check -p`: the `-p` intersection drops the excluded package from that
sweep, and a sweep left with nothing is excluded from the test phase -
`-p <excluded>` never becomes `cargo test --doc -p <excluded>`. This is a
contract, pinned by `cli_package_excluded_from_doc_only_sweep_skips_the_lane`.

### Parallel test binaries

A `[[check]]` entry carrying `parallel = { budget = N }` runs its test binaries
concurrently rather than letting cargo run them one at a time, under a budget of
tests in flight across the sweep. It exists for the wall-clock floor: cargo runs
binaries sequentially and `--test-threads` parallelizes only within one, so a
sweep cannot finish faster than the sum, over binaries, of each binary's slowest
test - a floor `test_threads` cannot move.

The sweep builds once, then fans out **by executing the prebuilt test binaries
directly**, never re-entering cargo, so what runs is exactly what the prebuild
built, on every selection shape (`test_exclude_packages` included). brokkr
reproduces cargo's launch contract for each binary (cwd, loader path, `[env]`,
build-script env, `CARGO_PKG_*`, `CARGO_BIN_EXE_*`; not `OUT_DIR`, which cargo
never gives a test). Consequences:

- A configured target runner or ambient build target refuses the lane (and
  every other direct-execution path, `brokkr test` included) rather than being
  bypassed. A sweep's own `target` key is the supported way to run for another
  triple; see `brokkr man sweeps`.
- Forwarded cargo args only cargo can honour (`--no-run`, `--target`,
  `--config`, `--manifest-path`, `--target-dir`) are rejected.
- Cargo target selectors after `--` (`--test NAME`, `--lib`, `--bins`, ...)
  narrow which binaries the sweep plans, unioned with the sweep's own `tests`
  and `lib_tests`.
- The sweep's `exclude_tests` then drops its targets from the plan, after the
  build: this lane, the process-isolated lane and the nextest lane (as a
  filterset term) are the ones that can, since a serial lane hands cargo its
  selection whole. An exclusion matching no integration-test target the lane
  built fails the lane, as a stale `tests` name fails cargo. The re-verification
  before the lane runs holds the same binaries out.
- On a sweep selecting exactly the whole workspace, `feature_unification =
  "auto"` still promotes the prebuild to workspace unification; an explicit
  `selected` is honoured.

Each binary claims a share of the budget proportional to its serial cost from
the last run (`.brokkr/parallel-timings.toml`), so the first run of a tree
weights by test count and budget changes need two runs to measure. An unset
budget (`parallel = {}`) is the physical cores in one last-level cache domain
(`brokkr env`'s `l3 domain:` line). Allocation rules and their rationale are in
`docs/brokkr.toml.sweeps.md` (`parallel`).

The lane writes its plan and result to the run log:

```
[run]     test fanout: 35 binaries, budget 24 in flight (claims 1-3)
[run]     test fanout: 4210 passed in 31.2s (35 binaries, built in 48.9s, slowest broadarrow-worker/ba-worker 28.7s)
```

On stdout its tests fold into the grouped test line, which names the slowest
parallel binary - the sweep's floor, and the one to split. When two or more
binaries each claim the whole budget (budget above 1) they could not overlap,
and that prints as a warning. A passing binary prints nothing of its own; a
failing one prints its label, a copy-pasteable repro command and its output,
reported after the join in plan order so concurrent failures never interleave.

Mutually exclusive with `isolation = "process"`. Doctests cannot run on a
parallel sweep: `[test] doctests = true` needs at least one non-parallel entry
(a load error), and a complete profile runs the sweep's doctests in a doctest
carrier of its build (see Doctests above).

## nextest (bundled; the `harness = "nextest"` lane)

**An engine, not a policy source.** brokkr links cargo-nextest's engine
(`nextest-runner`) as a fast process-per-test *executor* for a
brokkr-configured sweep - the reason is speed, the same reason `parallel`
exists: process-per-test dissolves per-test serialization, most dramatically
for a sweep that needs the `isolation = "process"` guarantee, which on the
libtest path is N sequential prebuilt-binary spawns and here is the
same guarantee executed concurrently. Linking rather than shelling out means
no per-host install step and typed `list::TestList` values with no JSON
round-trip; the engine version is brokkr's `Cargo.toml` pin, printed on the
lane's header line.

**Everything the lane runs and claims comes from `brokkr.toml`.** The engine
executes under a config brokkr *synthesizes* each run
(`.brokkr/nextest-synth.toml` under the state dir): retries zero, no
default-filter, no test groups, no setup scripts, and `slow-timeout` +
`terminate-after` set to brokkr's own 20s per-test watchdog ceiling - the
hang kill the other lanes have, per process, for free. The checkout's
`.config/nextest.toml` is **never opened** and `NEXTEST_PROFILE` is never
read: a foreign config must not get a vote in what a brokkr gate runs, and
an earlier version of this lane that honoured it had to grow a
pin/bless/drift apparatus just to police the vote it had granted.

A `[[check]]` entry selects the lane with `harness = "nextest"`
(`src/check_cmd/nextest_lane.rs`). The division of labour:

- **Brokkr owns the compile shape.** The build is brokkr's own
  `cargo test --no-run` with the sweep's selection, features, unification pin,
  cargo profile, `[lints]` allows and env - streamed into nextest's
  `BinaryListBuilder` - so a nextest lane and a libtest lane with equal
  compile inputs share the target dir and dedupe in clippy. `harness` is
  execution policy, never part of the build shape. The build's artifact
  stream must end in a `build-finished` record, for the run as for the
  coverage listing; a stream without one fails the lane rather than yielding
  an empty binary set.
- **Cargo's target runner is honoured, and a failure to resolve it fails the
  lane.** The engine resolves `[target.<triple>].runner` from the cargo
  config chain; when that resolution errors, the lane stops with the error
  rather than falling back to no runner, which would execute binaries
  directly where cargo would have wrapped them (qemu, wine, valgrind) - a
  wrong run whose output reveals nothing.
- **Brokkr owns the test env.** The sweep env the libtest lanes set on
  `cargo test` (`[[check]] env`, a profile's `env`, `BROKKR_TEST_BIN_DIR`,
  host-specific values like nidhogg's `CARGO_TARGET_TMPDIR`) reaches every
  test process and listing run the engine starts. The engine builds its own
  commands, so the pairs ride the one route it applies to all of them:
  cargo's `[env]` table, via a generated config file
  (`.brokkr/nextest-sweep-env.toml` under the state dir, rewritten per run)
  whose entries are `force = true` - a sweep value beats an inherited one,
  as it does on the libtest lanes.
- **Brokkr owns the filters.** The sweep's `only` and unqualified `skip`
  entries ride nextest's own libtest pattern emulation (identical substring
  semantics, nothing to escape); `include_ignored` maps to nextest's
  run-ignored policy; a package-qualified skip compiles to the term
  `not (package(P) & test(~X))` - folding the one predicate brokkr used to
  evaluate itself back into the tool. All of a sweep's qualified skips are
  ONE filterset, the conjunction of their terms: the engine ORs the filtersets
  it is handed (a test is admitted if any admits it), so one filterset per skip
  made disjoint skips admit everything. Filterset interpolation is restricted
  to identifier-ish characters and refuses anything else, because a
  wrongly-escaped filter silently changes which tests run. (`test(=X)`
  matches the **full** test path, which is why the translation uses `~`
  substring matchers - an exact-match translation of a bare name silently
  matches nothing.)
- **Brokkr owns concurrency.** The profile's `test_threads` maps onto the
  engine's in-flight count; unset (or 0) is the engine's num-cpus default.
  No serial-by-default here: process-per-test needs no watchdog-attribution
  trick, the synthesized `terminate-after` kills a hung test by name at any
  concurrency.
- **The engine owns execution and rendering** - process-per-test scheduling
  and its reporter. Brokkr adds the sweep bookends and decides pass/fail
  from the run's outcome. Every engine event - a start, a finish, a retry, the
  cancellation - is recorded for the execution accounting before the
  reporter sees it, so a reporter that fails cannot take the record with it.
  A death the lane's own fail-fast caused (a signal after the cancellation
  began) is recorded as `interrupted`, not as the test's failure; an
  exit-code failure stays the test's own; any engine timeout is `timed_out`,
  whatever the engine config would have made of it.
- **One listing, kept.** The `TestList` the lane's preparation builds is the
  one it executes; a second listing at run time could disagree with the plan
  about what the lane selects.

Refusals: trailing `-- -- <libtest args>` (the engine takes no raw libtest
argv; use the sweep's filters), the same forwarded cargo args the parallel
lane rejects, a `--format`-style libtest arg smuggled via a profile, and
filterset values outside the interpolation charset. Config composition rules
(load-time errors): `harness = "nextest"` with `parallel` (two execution
owners), with `feature_unification = "package"` (no per-package plan), or on
a profile that sets `isolation = "process"` (redundant - the engine already
runs every test in its own process; one spelling of the intent). `[test]
doctests = true` needs at least one plain libtest entry, since nextest never
executes doctests. Double-spawn is disabled (`NEXTEST_DOUBLE_SPAWN=0`
semantics): nextest's double-spawn protocol re-invokes the current
executable, which is brokkr, not cargo-nextest.

**What this lane does not replace:** the in-process libtest lanes stay the
default everywhere else. A shared-process lane is the only detector for the
process-global-state class (two tests contending over a global logger), and
process-per-test dissolves that contention along with the bug's visibility -
so moving a detector sweep onto the engine retires the detector. Use the
engine where isolation or per-test fan-out is the point, not as a blanket
replacement.

**The coverage audit covers nextest lanes**, so a `certifies = "complete"`
profile may reference them - the exclusion story is the existing ledger
model unchanged, because the synthesized config has no default-filter and
narrowing only ever comes from `brokkr.toml` skips and quarantines. How the
pieces fit (measured against the linked `nextest-runner` version recorded in
`Cargo.lock`):

- **A nextest lane's selection comes from the engine's own listing** under the
  sweep's real filters - the very `TestList` the lane executes, so plan and
  execution cannot disagree about what a sweep selects. `Selected` is the only
  executing verdict; pattern mismatches (`skip`/`only`) report `string` and
  filterset mismatches (qualified skips) report `expression`, both the
  ordinary needs-a-lane-or-a-quarantine verdict. A verdict the ledger has no
  policy for refuses the lane's preparation, `default-filter` included - under
  the synthesized config it cannot occur, so its appearance means a foreign
  config got a vote. The engine's binary ids are mapped onto brokkr's binary
  units through the lane's own artifact index; an id the index does not hold
  refuses the lane too.
- **Pairs are per binary on every lane**, nextest or not (see the `coverage`
  phase): two binaries in one package can define the same test path
  (measured: seven such collisions inside nautilus-infrastructure, all
  `serial_tests::` members), and the binary unit is what tells them apart.
  Package-qualified `[[quarantine]]` and skip entries mean "every binary of
  package X".
- **`MismatchReason` is priority-ordered, not a partition.** A test that is both
  `#[ignore]`d and unmatched by a lane's filterset reports `ignored`, so on a
  single listing it cannot be detected as an orphan. brokkr takes one listing and
  inherits that precedence: the only thing a second `--run-ignored all` listing
  would detect is a test that is both ignored and uncovered, which needs neither
  covering nor quarantining. The scheme self-heals when it matters - remove the
  `#[ignore]` and the verdict flips, the pair orphans, and the gate fails.
- **The universe comes from the shape's own per-binary enumeration**, never
  from a lane's listing: a package-scoped filterset reports the excluded
  package's suite as `skipped` with an empty testcase map, silently shrinking
  the universe. Lane listings are read only for their selections.

## `coverage` phase (complete profiles)

Every run that runs tests prepares the whole invocation before its first test
(the execution inventory, see "Diagnostic continuation"); under
`certifies = "complete"` the test phase is bracketed by `prepare`, which also
plans the policy universe, and `coverage` after it, which reconciles the plan
against what was observed. Two claims come out, and they are kept apart
because they are different claims:

- **Policy coverage**: every (shape, test) pair the profile could run is
  *selected* by some lane, or legitimately excluded (ignored, quarantined,
  curated). A property of the configuration, read from the plan.
- **Execution accounting**: every execution the plan expects ends as exactly
  one outcome - `passed`, `failed`, `timed_out`, `interrupted`, `ignored`,
  `unobserved` - taken from the typed observations the runners stream.

A timeout is an accounted failure, never coverage passed. `interrupted` and
`unobserved` are unresolved. A record that is complete after a timeout
certifies only that the failed run's record is complete, never the gate.

### What `prepare` does

Before any test runs, every attempted lane is built (support builds first, then
its shape, `cargo test --no-run`) and every binary is listed with nothing
compiling alongside. The plan - selected executions per lane, each test
executable's content hash, the support executables - is written to
`.brokkr/accounting/<run id>/plan.json`, and a journal beside it records every
observation as it happens, so it survives a watchdog kill. Only the newest few
run records are kept.

A lane that runs its binaries directly (parallel, isolated, nextest) re-hashes
them before it runs; if they still match it runs them with no cargo call. If
they changed, the lane is rebuilt and checked against the plan, and under a
complete claim any difference is a hard error ("artifacts changed since the
plan"), never a silent re-plan.

Under a complete claim, a lane that cannot be prepared leaves the plan
incomplete and no tests run (`failed_phase: "prepare"`). Two lane shapes cannot
be attributed and are refused there: a serial lane whose harness isolation
cannot be installed, and cargo-mediated parallelism (`test_threads` other than
1; use `parallel = { budget = N }` instead).

**The green invariant**: the plan is complete, every expected execution
`passed`, nothing anomalous was seen (duplicate or unplanned records, totals
that disagree, a suite left open), no termination is on the record, and every
planned doctest carrier completed. `ignored` and `interrupted` both fail it.
Exit codes are unchanged: a fired ceiling exits 124, an interrupt 130.

Identities (binary unit, pair, execution), the build/inventory/policy passes,
the outcome evidence table, anomaly list and doctest carrier rules are in
`reference/check-internals.md`.

### Policy coverage

Classification reads the plan: a shape resolution's selection is the union of
its lanes' executions. That deliberately changes how a failing run is
classified - a lane the fail-fast never reached still selects its pairs, so
they are not orphaned; its executions are `unobserved` in the accounting. An
incomplete plan reports what it knows - the orphans of shapes whose every
lane was prepared, the dead filters whose every sweep was listed - with
status `incomplete`, never a pass; stale quarantine entries are not judged
there, since an entry matching nothing may be matching in a shape that could
not be prepared. `curated = true` entries are the declared narrowing of the
universe (below); package-level `test_exclude_packages` is the other.

The universe is **every `[[check]]` entry**, not the profile's own sweep
list: if it were the latter, dropping a sweep from a lane would silently
shrink the certified set. A `complete` profile that leaves a `[[check]]`
entry referenced by no sweep or lane is therefore a **load-time error**
(the entry would be enumerated nowhere, so the audit would print `0
orphaned` over tests that never ran) - **unless the entry declares
`curated = true`** (see `docs/brokkr.toml.sweeps.md`): a curated entry's
non-selected pairs are outside the universe by declaration, so leaving it
unreferenced certifies nothing without running, and the entry may live in
its own deliberately-run profile instead. When a gate lane does run a curated
entry, its build shape's non-selected pairs are exempted rather than audited
- counted and trailer-reported like `test_exclude_packages`, never orphaned,
never credited to a `[[quarantine]]` entry. The exemption is keyed on the
sweeps, not the shape: a non-curated entry sharing a build shape with a
curated one keeps that shape fully audited. The universe and each lane's
selection come from the binaries' own libtest listings, under the lane's real
filters, so libtest itself decides what a filter admits. A shape's universe
is its `--tests` set plus every target a lane of the shape selects
explicitly (`tests`, `lib_tests`), so a `test = false` target a lane runs is
audited like any other. Every non-selected
pair must be quarantined (`[[quarantine]]` pattern match, optionally
package-scoped - meaning every binary of that package - counted per entry -
the **most-specific** matching pattern takes the pair, so a narrow entry is
never starved by a broad one it nests under) or ignored at the source
(counted, reported, not fatal); anything else is **orphaned** and fails the
check, listed in full as `shape/binary/test`. A pattern entry justifying zero
pairs is stale and fails the check. A run with stale entries, orphans and
dead filters (below) prints **all** the worksheets before failing.
Package-level `test_exclude_packages` is outside the pair audit (those
binaries cannot build) and is called out in the trailer.

The ledger reports as a per-issue pair breakdown in descending order. On a
passing audit it rides on the coverage line's `quarantined` field (`106
quarantined (B51 80, B41 14, B50 10, ...)`), whose count is the same pair
total; on a failing or incomplete audit, where that line does not print, it is
one rolled-up line of its own ahead of the findings (`quarantine: 21 entries,
106 pairs - B51 80, B41 14, B50 10, ...`). That keeps both signals
the per-entry listing carried: the countdown, and the growth warning when a
substring starts matching more than it used to. It is a summary, not a cap -
every pair the ledger holds is counted in the line, at the granularity a
reader acts on (an issue, not the pattern under it).

### The summary objects

The `--json` summary (schema 2) carries the two claims as separate objects,
plus the doctest block and the run's termination:

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

A doctest carrier entry also lists, when non-empty, `missing` - the cargo
resolutions it owed a rustdoc stream that presented none (a package name, or
`the lane's selection` for a combined one) - and `declined`, the libraries a
derived carrier declined for their manifest's `doctest = false`.

`policy_coverage.status` is `passed`, `failed` (orphans, stale entries or
dead filters) or `incomplete`; `dead_filters` counts the dead `skip`/`only`
filters below, a field of its own because a dead filter moves no pair
between the other buckets. The execution counts partition the expected
executions; `anomalies` is counted beside them. All three blocks are present
whenever the `prepare` phase ran - on a failed run too, a consumer of a failed
gate sees the worksheet's numbers, not `null`. They are `null` outside a
complete profile, and on a complete-profile run that failed before `prepare`
(a gremlin, a clippy error, a pre-test script check): no plan exists there,
and a block of zeros would read as an empty plan rather than no plan.
`execution_accounting` is deliberately not populated for a run that certifies
nothing, though it has an inventory too: its scope there would need stating on
every field, and `diagnostic_continuation.inventory` states it once.

**Termination** - `{kind, scope}`, `null` when nothing stopped the run - is
how the run stopped: `phase_deadline` (scope: the phase), `interrupt`,
`per_test_timeout`, `run_deadline`, `sibling_timeout`, `fail_fast` or
`engine_error`, with the lane it covered as the scope where it was a lane's.
When a lane's tests or record fail, the fail-fast it causes is scoped to the
first later lane it stopped, never one the selection excluded. Failing that
way in the last attempted lane stops nothing, so `termination` is `null`
unless the lane recorded its own (a timeout, say), and the error output names
the failed sweep. An operational error that ends the phase (a refusal, a
failed pre-build) is the run's, with scope `run`.
Present outside complete profiles too.

## Diagnostic continuation

After a watchdog kill, a per-test timeout, an interrupt or a fail-fast, `check`
and `brokkr test` account for every test the run left without a verdict (naming
each, except plain fail-fast casualties, which are counted). The report is a
diagnostic: it reruns nothing, and a failed run never turns green because of
it. The motivating case: a downstream
merged its integration tests into one binary per crate, so one hang in a
serial lane under a *partial* profile - the bare `brokkr check` - hid a whole
crate's results.

It works because every run that runs tests - partial profiles and `brokkr test`
included - prepares an execution inventory up front (`prepare`, above), without
the policy universe a complete claim adds. A lane that cannot be attributed to
binaries (cargo-mediated parallelism, no harness isolation) has an
**unavailable** inventory, and only the suspects its shared stream blamed can
be named. A binary that answers no libtest `--list` is **unenumerable** and its
lane's inventory is **partial**. Outside a complete claim, preparation failures
are recorded and the lane runs as it always did. A `harness = false` target in
a partial profile's serial lane is never executed to ask for a listing.
Inventory rules, preparation costs and `brokkr test`'s per-sweep recheck are in
`reference/check-internals.md`.

### The report

On the final reporting path, after the failure verdict and before the `--json`
trailer, every unresolved execution (`interrupted` or `unobserved`; failed,
timed-out, passed and ignored stand as the results they are) is counted in the
header, and every one that a kill, a hang, a deadline or an interrupt left is
named - no cap - grouped lane, resolution, binary, with its outcome and detail:

```
diagnostic continuation: 3 unresolved executions (of 3 expected)

default / crate-a / test:integration
  interrupted  detector::alpha  phase deadline
  unobserved   detector::beta   phase deadline
  unobserved   detector::gamma  phase deadline

note: diagnostic only: ...
note: an unobserved execution may have run if records were lost; ...
```

The one exception is the plain fail-fast casualty: an execution that never
started only because an earlier failure stopped the run (`unobserved`, detail
`fail_fast`). The text prints one line per binary with a count (`default /
crate-a / test:later: not reached, 40 tests` for a binary that never started,
or `  not reached: 40 tests` under the heading of a binary that did start or
also has named executions) instead of the names. The
motivating case for naming everything was a hang, where each unresolved test is
a suspect; for an ordinary assertion failure the list is almost entirely
fail-fast casualties burying the failure that caused them. The full names stay
in the `--json` `candidates`, and a fail-fast victim that was *interrupted*
(started, then cut down) is still named. A binary of which nothing was
observed and that has named executions is marked `(not reached)`. A lane whose
inventory is unavailable or partial says so under the list. The text ends with
the statement (diagnostic only, certifies nothing, never changes the verdict)
and the evidence caveat; the statement also stays in the `--json` trailer's
`statement`. On a failed run whose continuation prints, the `accounting:`
counts line is log-only, since the header carries the unresolved and expected
counts; anomalies and journal problems still print.

`--json` (schema 2, additive) gains `diagnostic_continuation`, `null` on a run
that passed, left nothing unresolved, or failed before `prepare`:

```json
"diagnostic_continuation": {
  "source_run_id": "123456-2",
  "certifies": false,
  "statement": "diagnostic only: ...",
  "inventory": {
    "scope": "binary_tests", "availability": "complete | partial | unavailable",
    "plan_complete": true, "journal_closed": false,
    "expected_executions": 0, "unresolved": 3, "anomalies": 0,
    "lanes": [ { "lane": 0, "label": "default", "kind": "serial",
                 "availability": "complete | partial | unavailable | skipped | doctests",
                 "reason": null, "expected_executions": 0 } ],
    "suspects": []
  },
  "candidates": [ { "lane": "default", "resolution": null, "package": "crate-a",
                    "binary": "test:integration", "test": "detector::alpha",
                    "outcome": "interrupted", "detail": "phase_deadline",
                    "execution": { "lane": 0, "attempt": 1, "pair": { } } } ]
}
```

`candidates[].execution.pair.shape` is an opaque declared-shape digest. Its
versioned encoding can change the digest value between brokkr versions; the
schema and field remain unchanged, and the digest is not a cross-run key.

`policy_coverage` stays `null` for a run that certifies nothing, and
`execution_accounting` is not populated there either (see "The summary
objects"): the inventory's scope is stated once, here.

What the words mean, and no more:

- `timed_out`: the execution exceeded its attributed budget - not proven to be
  the cause of a hang.
- `interrupted`: a start was observed without an acceptable terminal.
- `unobserved`: no start and no terminal were recorded. It may have run if
  records were lost; missing evidence is not proof of non-execution.

A harness that passes every test and then exits nonzero (or dies by a signal)
with nothing else to explain it is an `unclean_exit` anomaly, so the run is not
green; a nonzero exit that a failed test, a kill or a stop explains is not
counted twice.

## Feature unification

A `[[check]]` entry chooses which feature graph its cargo invocations resolve
against:

| Value | Meaning |
|---|---|
| `auto` (default) | Brokkr policy: pin nothing, except the parallel lane's whole-workspace promotion described above. Exactly what every sweep did before this key existed. |
| `selected` | Pin cargo's default - each selected package resolved with only the features that selection requires. |
| `workspace` | Pin workspace-wide unification: every member contributes features, whatever the selection. |
| `package` | Pin per-package resolution, the shape `cargo install` produces. Requires a non-empty `packages`. |

The three explicit values mirror cargo's own `resolver.feature-unification`
vocabulary, and each **pins** its value on the argv (`-Zfeature-unification`
plus a `--config`), so a lane's resolution does not depend on whether the
consuming repository happens to carry a matching `[resolver]` block in
`.cargo/config.toml`. A CLI `--config` outranks a config file, so the pin wins.
`auto` is the one value that defers to ambient config, which is why the default
is spelled `auto` rather than `selected`: absence and `auto` mean the same
thing, so materializing the default cannot change behaviour.

### What `package` catches

The `member_check` phase below compiles the install shape but runs no tests,
and the defect class it cannot see is a **runtime** one: a crate whose behaviour
depends on a feature a sibling workspace member enables. A daemon taking a
dependency with `default-features = false` and calling a `#[cfg(feature)]`-gated
path compiles identically either way; under workspace resolution a sibling
donates the feature and every dev build passes, while every installed binary
takes the other arm. Only running that package's tests in package resolution
distinguishes them.

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
`[bin] install`: a crate's install shape and its adapter-free consumer shape are
different questions, and only the first could be derived.

### Consequences of package mode

- **One cargo invocation per package**, never a batched multi-`-p` - the same
  rule, and the same measured reason, as the `member_check` phase below.
  This applies to clippy, the prebuild, the test run, the parallel fan-out and
  the `prepare` phase's enumeration alike.
- **Its own target dir** (`<target>/unify-package[-<rustflags hash>]`), because
  a different resolution re-fingerprints units and would otherwise rebuild
  against every ordinary sweep on every run, in both directions. The dir is
  shared across the lane's per-package resolutions: cargo fingerprints
  independent invocations correctly within one directory, and a lane exports a
  single `BROKKR_TEST_BIN_DIR`, so a directory per package would leave a
  multi-package lane with no one place holding the binaries its tests spawn.
- **Its own coverage shape per resolution.** Under `certifies = "complete"` a
  two-package package-mode lane audits as two shape resolutions, labelled
  `<entry>/<package>`, its pairs carrying the resolution. It does *not* need
  `curated = true`: package scoping narrows the universe and the selection
  together, so an unfiltered lane selects its whole universe and leaves no
  orphans.
- **Forwarded package selectors are refused.** `brokkr check -- -p other` under
  package mode is an error, because cargo unions selection flags and would
  widen a resolution promised to hold exactly one package.
- **Each resolution carries only its package's features.** An entry's
  package-qualified tokens are projected onto each resolution's one package,
  so `strategy/hotpath` does not reach the `vm` resolution - see
  [Feature projection](check.md#feature-projection).
- **Each resolution carries only its package's target selectors.** A
  `--lib` / `--test <name>` reaches a resolution's prebuild only when its
  package has that target (cargo refuses one it lacks), so an explicitly
  selected `test = false` target is built there as everywhere else. A package
  with none of the selected targets builds its default set, which the
  selectors then narrow to nothing.
- The mode is part of the **build shape** (and of what `brokkr test` dedupes
  on), so a package-mode and a workspace-mode lane never dedupe into each
  other in clippy, in `brokkr test`, or in the coverage audit.

## `member_check` phase (each member resolved alone)

Every other compiling phase resolves features over a selection that includes
sibling workspace members, so a shared dependency's features are the union of
what all members ask for. `cargo install -p <member>` - and `brokkr install`
per `[bin] install` package - resolves each package **alone**. A call site
that only compiles because a sibling enables a third-party feature (a tokio,
hyper, or upstream-crate feature, not just the workspace's own) is green under
every workspace-selected sweep and fails at install time, on the deploy path.

The phase closes that gap: **one `cargo check` per workspace member** under
`-Zfeature-unification` with `resolver.feature-unification="package"` -
cargo's mode that resolves a selected package as though it were selected
alone, which is exactly `cargo install`'s boundary - with `--locked`,
`--keep-going`, the configured profile, and the `[lints]` allows through the
same rustflags plumbing as the test phase. The CLI `--config` outranks a
repo's committed `.cargo/config.toml`, so a workspace-mode unification pin
(adopted for the parallel lane) and this phase coexist. Every member is
checked even after an earlier one fails, so one run reports the whole
workspace. The member builds and their `--bin NAME` eligibility probes all
use the `unify-package` target directory beside Cargo's metadata-resolved
target dir, keeping this package graph out of the workspace-shared cache
while every member and every run reuse it incrementally.

```toml
[member_check]
when = "gate"               # "off" | "gate" (default) | "always"
targets = ["lib", "bins"]   # default; any non-empty subset
profile = "dev"             # "dev" (default) | "release"
skip = [{ package = "xtask", reason = "tooling, never installed" }]
```

- **Members** are cargo metadata's `workspace_members` (so `exclude` is
  respected) less `skip`. Every `skip` entry carries a reason, and one naming
  no member fails the phase - validated against the whole member set before
  any `-p` narrowing, so a narrowed run cannot hide a stale entry. There is
  deliberately no allowlist: a member added later is checked, not silently
  left out.
- **Production targets only.** `targets` spells `lib` and `bins`, nothing
  else. Tests, benches and examples pull the member's dev-dependencies into
  the same resolve, and a dev-dependency can donate a feature to a dependency
  the production targets also use - package unification isolates members from
  each other, not a member's targets from each other. A member with no target
  of a selected class is fine as long as it has one of the other; a member
  with none at all fails (skip it with a reason).
- **Profile** is part of the claim and named on the ok line: code can differ
  under `debug_assertions`, so a dev check says nothing about release.

Per package rather than one batched multi-`-p` invocation, from a measured
false positive: cargo dedupes units across a batched graph, so with two
feature variants of a shared dependency in the batch, a path dependency
whose own features do not vary dedupes onto one variant while a crate
between them compiles against both - producing "expected `AccountId`, found
`AccountId`" errors in an innocent intermediate crate, a shape no actual
`cargo install` can resolve its way into. Per-package invocations are the
advertised boundary, and they attribute every error to the member whose
resolve produced it. When one resolve builds more than one feature set of a
crate version, the failure output names the crate as a candidate cause -
rustc's "mismatched types" between identical type names otherwise reads as
version skew that is not there. It is only a candidate: host and target
dependency domains may legitimately differ.

CLI `-p` intersects with the member set: named members are checked, a
skipped or non-member name is dropped with a note, and an emptied
intersection skips the phase visibly. (Moot under `--gate`, which refuses
`-p` outright.)

What a green run claims, precisely: **each checked member's selected
production targets compile under that member's own resolve, at the named
profile, against the workspace lockfile.** It does not claim the member will
`cargo install` - the check performs no codegen or linking. It shares a
dependency graph with `brokkr install`, which passes `--locked` (`brokkr man
run`). Raw `cargo install --path`, and `brokkr install --unlocked`,
re-resolve instead - and then this phase's green says nothing about the
versions they pick.

Two refusals keep it honest:

- **Zero-target refusal.** `--bins` silently skips a bin whose
  `required-features` are unsatisfied, so the artifact stream is compared
  against each member's declared production targets, keyed by the member's
  exact package id, target class and name, and crediting only non-test
  compiles - a gate that checked nothing must not read as green. A lib
  missing from a successful stream fails (a lib has no `required-features` to
  excuse it). A bin missing from the stream is put to cargo by name (`--bin
  NAME`, every other resolution argument identical): a build whose stream
  carries the bin's artifact credits it, a compile error fails the member,
  and cargo's own
  `requires the features` refusal *naming that target* waives it, since
  `cargo install` would skip it too. Anything else fails closed. Brokkr
  deliberately does not decide eligibility from `required-features` itself -
  that would mean reproducing namespaced features, `dep:` suppression of
  implicit features, weak dependency features and `dep/feat` requirements,
  and a model wrong in the permissive direction drops a bin from the
  expectations, never compiles it, and reports green: cargo's silent skip
  moved inside brokkr, where nothing is left to catch it. Waived bins are
  reported by name, and a member whose targets are *all* waived still fails.
- **No toolchain fallback.** A cargo without package-mode support fails with
  a named remedy (update nightly, or turn the phase off - `[member_check]
  when = "off"`, or `[bin] install_feature_check = "off"` for the install gate);
  falling back to selected-mode would silently recreate the workspace-unified
  graph the phase exists to leave. That classification aborts the phase, so it
  requires a positive match on cargo's own option rejection *and* the absence
  of rustc diagnostics - a substring test for "nightly" matches ordinary
  compile output and a dependency named `nightly-helper` alike, and would
  abort the remaining members with a false remedy.

On failure, the errors are ordinary rustc errors against code that compiles
fine in the rest of the run - genuinely confusing without context - so the
phase offers a candidate explanation: it diffs the workspace resolve against
the failing member's solo resolve (`cargo tree`'s normal+build graph with
default features - an approximation of the failing build, labelled as one)
and prints which features each dependency loses ("hyper v1.11.0: server").
When one of those is what the code needs, the fix is declaring it in the
member's own manifest. The errors print in the one-line form like every other
phase; the low-noise debug loop is `-p <member>`, which narrows the test
phases and the member set together.

When it runs: `when = "off" | "gate" | "always"`, default `gate` - only under
a `certifies = "complete"` profile, because per-member resolution
deliberately compiles duplicate variants of shared dependencies and is priced
for the pre-landing run, not the edit loop.

Without a `[member_check]` table, a non-empty `[bin] install` implies the
install gate it has always implied: the phase runs over the install packages
only, their bins only, at the profile `brokkr install` builds (`[bin] debug`),
at `[bin] install_feature_check`'s cadence (default `gate`). A package there
that is not a workspace member fails the phase. A `[member_check]` table
supersedes the implied gate - every member, its own targets, profile and
`when` - so `install_feature_check` beside one is a load error. With neither,
the phase is inert. A partial profile may name `member_check` in
`skip_phases`; the phase's former name, `install_feature`, is accepted there
as the same phase. The phase budget
is the test phase's (it covers a whole workspace of cold resolves); the
whole-check ceiling still bounds the run.

## Dead `skip` / `only` filters

The same staleness rule the ledger applies to `[[quarantine]]` entries
applies to the filters themselves: **a filter that matches no test is a
defect, not a no-op.** During the `prepare` phase every `skip` and every
`only` declared on a `[test.profiles.*]` block or a `[[check]]` entry is
asserted against the enumeration, and one that matched nothing fails the
check in the `coverage` phase, named with the block it was written in:

```
[error]   dead filter: only "read_market_latency" in [test.profiles.timing] - matches no test in any sweep it applies to (timing)
```

A dead `skip` is a name that drifted: whatever it excluded runs again under
a name nobody wrote down, or it will silently start catching an unrelated
test that grows into the substring later. A dead `only` is worse, because
the lane then evaluates nothing at all - a sweep declared to carry a
wall-clock contract, whose filter no longer matches, is a gate that has
quietly stopped existing while still sitting in the config as evidence that
the contract is checked. Neither is visible to the orphan audit: a dead
skip subtracts nothing from the lane's claim, so no pair goes non-run and
nothing is orphaned.

Four rules decide what "matched nothing" means:

- **Judged against the lane's binaries, not the shape's universe.** A lane
  narrows binaries with `tests = [...]` (`--test <target>`), `lib_tests` and
  `exclude_tests`, so a filter is matched against the names *that lane* can
  see - an `only` that matches nothing outside an excluded target is dead. Against the wider universe
  a skip would read as alive on the strength of a match inside a binary the
  lane narrowed away.
- **The reference set is the filter's own scope**, which is the union of
  the candidate sets of the sweeps it applies to: a `[[check]]` filter
  against the lanes running *that entry*, a `[test.profiles.*]` filter
  against *every sweep the profile runs*. The two claims differ - "this
  test should not run in this sweep" versus "...in this profile" - and the
  latter is satisfied by matching anywhere the profile runs. Judging a
  profile filter per sweep is wrong in a way that shows up immediately:
  any profile combining an unscoped sweep with a package-scoped one
  reports a false death for essentially every entry, since each skip names
  a test outside the scoped sweep's packages and is necessarily dead there
  while doing its job in the unscoped one. Nothing is lost in the
  direction the check exists for - a filter dead in *every* sweep it
  applies to has no live sighting anywhere, and still reports.
- **`skip` and `only` run against different sets.** A `skip` is dead when
  nothing it could remove *exists*. An `only` is dead when the lane
  *evaluates* nothing under it, so the skips and (on a lane without
  `include_ignored`) the `#[ignore]`d names come out first - an `only`
  whose every match is skipped or ignored satisfies "matched something"
  while selecting no work.
- **Each filter is asserted individually.** libtest ORs positional filters,
  so a lane with one live `only` and one dead one still runs tests and
  looks healthy. Folding the assertion the way libtest folds the filters
  would let the live sibling cover for the dead one.
- **Package-qualified skips match within their package only**, exactly as
  `package = "<pkg>"` scopes a `[[quarantine]]` entry.

Every prepared lane is judged, whether or not the test phase reached it: a
filter's liveness is a property of what the lane selects, which the plan
knows before anything runs. A filter one of whose sweeps could not be
prepared is unknown rather than dead - the sweep nobody listed may be exactly
where it matches - and is not reported.

Resolving filters against libtest's own enumeration is a correctness
choice, not only a cheap one. The alternative - deriving test names by
parsing Rust source - has to be taught every declaration form
(`#[tokio::test]`, `macro_rules!`-generated tests, and so on), and a parser
that has gone blind agrees with every filter list there is.

### The two guards are complementary

The alive-check runs only under `certifies = "complete"`, because that is
the only place the `coverage` phase runs. **That is not the same as "dead
filters go unchecked elsewhere"** - a second guard covers the other
direction. When a sweep actually runs and its filters collect no work, the
test phase already refuses:

```
cargo test: zero tests ran (sweep: timing) (1 suite, 99 filtered out)
  - a profile/filter combo collected no work; treat as a wrong-run.
```

So a dead `only` is caught at **run time** in any sweep that runs, and the
alive-check covers the **enumerate-but-don't-run** case: a curated entry
referenced by a complete profile, whose lane is enumerated by the audit.
The residual between them is a sweep that is neither enumerated under a
complete profile nor ever run - which is a lane nobody evaluates, a larger
and different defect, and not one a filter check can fix.

The degenerate-filter floor below applies everywhere, under every profile,
because it is enforced at config load rather than in either phase.

### The minimum filter length

A `skip` or `only` substring shorter than **four characters** is a
**load-time error**, in any `[[check]]` entry or `[test.profiles.*]` block
(for a package-qualified skip, the floor is on the `pattern` half; the
`package` half is an exact name):

```
[[check]] entry 'unit' has `skip` filter "ser", shorter than 4 characters. ...
```

It closes the hazard the alive-check structurally cannot see. A very short
substring is a substring of nearly every test name, so it suppresses (or
selects) tests nobody chose *while always matching something* - it
satisfies "matched at least one test" vacuously. The alive-check catches a
filter that matches too little; the floor catches one that matches too
much.

It is enforced at load rather than in the `coverage` phase for the reason
in the section above: the phase runs only under a complete profile, and a
degenerate filter is degenerate wherever it is declared. Loading is also
where the config location is known exactly. The escape hatch for a
genuinely broad filter is a longer substring - there is no opt-in flag.

> [!WARNING]
> This is a breaking change for existing configs. The floor is a parse
> error, so a `brokkr.toml` carrying a three-character filter stops loading
> rather than warning - every command fails, not just `check`. Fix is to
> write the substring that was meant.

## Env vars exported to `cargo test`

Both `brokkr check` (test phase) and `brokkr test` set the following on every
`cargo test` invocation, including sweeps with empty `build_packages`:

- `BROKKR_TEST_BIN_DIR` - directory containing the just-rebuilt
  `build_packages` artefacts. `brokkr check` sets it to `<target>/debug` (the
  test phase runs without `--release`) unless the sweep carries
  `profile = "release"`, in which case it sets `<target>/release` and the
  whole sweep - clippy, pre-build, test run, coverage enumeration - compiles
  release; `brokkr test` sets `<target>/release` by default, `<target>/debug`
  when `--debug` or `[test] debug` applies, and follows the sweep's own
  `profile` when it declares one. The profile tracks the cargo invocation 1:1 - it does
  *not* track whatever profile cargo happens to compile the test harness with.
  `<target>` comes from `cargo metadata --no-deps`. A sweep with a `target`
  triple gets `<target>/<triple>/<profile>`, where its own artifacts land;
  host units cargo builds alongside (a proc-macro's test harness) receive the
  same directory. Tests that spawn the
  rebuilt binary should read this var as the primary source of truth and fall
  back to `cfg!(debug_assertions)` only when it's unset (e.g. plain
  `cargo test` outside brokkr). The `cfg!(debug_assertions)` heuristic is
  unreliable because `[profile.test]` overrides can flip
  `debug-assertions = false` in the test binary even though the rebuilt
  binary lives under `debug/`.
