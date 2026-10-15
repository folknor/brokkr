# brokkr.toml: sweeps, test profiles and quarantine

The brokkr.toml sections that decide what `brokkr check` and `brokkr test`
build and run: `[[check]]` sweeps, the `parallel` lane, `[test]` and its
profiles, and the `[[quarantine]]` ledger. The rest of the schema is in
`docs/brokkr.toml.md` (`brokkr man config`); how the lanes run is in
`brokkr man test-lanes`.

## `[[check]]` array

Optional. Each entry is one (clippy + test) sweep with the entry's feature
flags. Profiles in `[test.profiles]` reference these by name.

```toml
[[check]]
name = "all"
features = ["test-hooks", "linux-direct-io", "linux-io-uring", "commands"]
build_packages = ["pbfhogg-cli"]

[[check]]
name = "consumer"
no_default_features = true
features = ["commands"]
build_packages = ["pbfhogg-cli"]

# Virtual workspace (no root package): scope with `packages` so `--features`
# is legal, and pin a build-affecting var for the whole sweep with `env`.
[[check]]
name = "core"
packages = ["nautilus-core", "nautilus-common"]
features = ["high-precision"]
env = { HIGH_PRECISION = "1" }
```

- `name` (required) - label surfaced in output and the key profiles use to
  reference this entry. Must be unique.
- `features` (optional, default `[]`) - explicit list of cargo features. The
  `features = "all"` sentinel (which used to mean `--all-features`) is
  rejected; enumerate features explicitly so adding a new feature to
  `Cargo.toml` doesn't silently broaden the test sweep. Entries may be
  package-qualified (`piners-vm/opcode-counts`), and are written against the
  entry's own selection: a run that selects fewer packages - `brokkr check -p`,
  `brokkr test`'s one package, each resolution of `feature_unification =
  "package"`, each `build_packages` pre-build - drops (and announces) exactly
  the tokens that only packages outside its concrete selection route. Every
  other token is kept and reaches cargo as written: unrecognized tokens
  (nothing in the entry's selection routes them), malformed ones (`dep:x`,
  `a/b/c`), and `dep/feature` requests whose dependency a selected package
  declares even when that dependency lacks the feature - cargo rejects those
  with its own message, as the full run would. See `brokkr man check
  feature-projection` for the routing rule.
- `no_default_features` (optional, default `false`) - emits
  `--no-default-features`.
- `build_packages` (optional, default `[]`) - cargo packages rebuilt with the
  entry's feature flags (projected onto each package) before the test phase. Required when `tests/cli_*.rs`
  integration tests invoke a separate CLI workspace member, otherwise
  `cargo test -p <lib>` leaves the binary in whatever state it was last built
  and the consumer-sweep contract goes unverified.
- `packages` (optional, default `[]`) - packages the sweep is scoped to,
  emitted as `-p <pkg>` on both `cargo clippy` and `cargo test`. Required to
  use `features` in a **virtual workspace** (one with no root package): cargo
  rejects `--features` at the workspace root, so the sweep must name the
  package(s) the features belong to. Distinct from `build_packages`, which
  only pre-builds CLI binaries; `packages` scopes the check itself.
- `test_exclude_packages` (optional, default `[]`) - packages to omit from
  the **test phase only**, emitted as `cargo test --workspace --exclude <pkg>`.
  Clippy still runs workspace-wide. For a workspace member whose test binary
  can't link in this environment (e.g. it needs a system library the build
  host lacks) and would otherwise fail the whole test phase. Mutually exclusive
  with `packages` (you can't both `-p`-select and `--workspace`-exclude);
  setting both is a parse error.
- `env` (optional, default `{}`) - environment variables exported to *every*
  cargo subprocess the sweep runs: clippy, the test-phase pre-build, and the
  test run. Use it to pin a build-affecting toggle (e.g. a codegen flag whose
  drift you'd otherwise catch only in `git status`) so `brokkr check` is
  reproducible without exporting it by hand. Merged under a referencing
  profile's `env`, with the entry winning on a key collision.
- `rustflags` (optional, default `[]`) - extra `rustc` flags (token list, e.g.
  `["--cfg", "madsim"]`) exported as `RUSTFLAGS` on the sweep's cargo processes
  only, **composed** with any inherited `RUSTFLAGS` (appended; falls back to
  `CARGO_ENCODED_RUSTFLAGS` when the environment already carries the encoded
  form). A non-empty value **auto-isolates** the sweep into its own target dir,
  `target/rustflags-<hash>`, keyed by the flag content: a global cfg like
  `--cfg madsim` reshapes every fingerprint in the build graph, so sharing the
  default `target/` would force a full recompile in both directions on every
  alternation with the plain sweeps. Isolation keeps the caches apart; sweeps
  carrying *identical* flags share one dir (so several madsim legs compile the
  simulator once). There is no key to set - isolation is automatic and derived
  from the flags. Setting `RUSTFLAGS` or `CARGO_TARGET_DIR` in `env` alongside
  `rustflags` is a parse error (one unambiguous source each).
- `feature_unification` (optional, default `"auto"`) - which feature graph this
  sweep's cargo invocations resolve against: `"auto"`, `"selected"`,
  `"workspace"` or `"package"`. The three explicit values mirror cargo's own
  `resolver.feature-unification` vocabulary and **pin** their value on the argv
  (`-Zfeature-unification` plus a `--config`, which outranks a repo's committed
  `.cargo/config.toml`), so a lane's resolution never depends on an ambient
  config line. `"auto"` is brokkr policy rather than a cargo mode: it pins
  nothing, except that the parallel lane still promotes an eligible
  whole-workspace fan-out to `workspace` - exactly what every sweep did before
  this key existed, which is why absence and `"auto"` mean the same thing.
  `"package"` is the `cargo install` boundary and the only mode that catches a
  crate depending on features a sibling member donates; it requires a non-empty
  `packages`, resolves one cargo invocation per package, and **auto-isolates**
  the sweep into `target/unify-package[-<rustflags hash>]` for the same
  fingerprint-thrash reason `rustflags` does. An explicit `"selected"` is
  honoured everywhere, parallel whole-workspace sweeps included - the fan-out
  executes the prebuilt binaries directly, so the prebuild's resolution is the
  run's resolution. See `brokkr man test-lanes feature-unification`.
- `harness` (optional, default `"libtest"`) - which engine executes the
  sweep's test phase. `"libtest"` is brokkr's own lanes (serial,
  `test_threads`, `parallel`, `isolation = "process"`). `"nextest"` runs the
  sweep through the linked nextest engine as a fast process-per-test
  *executor* - under a config brokkr synthesizes per run (retries zero, no
  default-filter, no test groups, and brokkr's own 20s per-test watchdog as
  `slow-timeout` + `terminate-after`). The checkout's `.config/nextest.toml`
  is never opened and `NEXTEST_PROFILE` is never read: everything the sweep
  runs and claims comes from `brokkr.toml`. Brokkr owns the compile shape
  (selection, features, unification pin, profile, `[lints]` allows),
  translates the sweep's `skip`/`only` into the engine's own filters, and
  maps the profile's `test_threads` onto the engine's in-flight count (unset
  = num-cpus), so `harness` is execution policy and never part of the build
  shape - a nextest and a libtest sweep of equal compile inputs share the
  target dir and dedupe in clippy. The natural use is a sweep needing the
  process-isolation guarantee, executed concurrently instead of one cargo
  spawn per test. Legal under a `certifies = "complete"` profile: the
  `prepare` phase takes a nextest lane's selection from the engine's own
  listing - the one the lane then executes - keyed per binary like every
  other lane, and a package-scoped `[[quarantine]]` entry spans every binary
  of its package. Load errors: combined with `parallel`,
  with `feature_unification = "package"`, or on a profile setting
  `isolation = "process"` (redundant - the engine already isolates). See
  `brokkr man test-lanes nextest`.
- `doc_only` (optional, default `false`) - the entry runs `cargo test --doc`
  (doctests, nothing else) with its selection, features, unification pin and
  cargo profile, serially. The doctest twin for a `parallel` workspace
  sweep: doctests live in cargo's `--doc` pseudo-target, which no binary
  fan-out can carry, so a project parallelizing its main sweep pairs it with
  a doc-only entry of the same selection. Runs doctests regardless of
  `[test] doctests` (the key is the entry's own opt-in), and a referenced
  doc-only entry satisfies a complete profile's doctest obligation even
  under `doctests = false` - a `category = "doctests"` quarantine coexisting
  with one is then a stale-entry load error. Owns cargo target selection
  outright (`--doc` is exclusive): entry `tests` and forwarded target
  selectors are refused, as are entry-level `skip`/`only` - doctests cannot
  be enumerated, so a filter on them could never be audited for liveness,
  and under a complete claim profile-inherited filters are refused too
  (compose the doc twin as its own filterless lane). Under a complete claim
  with a doctest obligation, at least one doctest carrier must be
  **workspace-shaped**: `packages` empty, with `test_exclude_packages`
  allowed and reported - a package-scoped twin would prove "some doctests
  ran" while the rest of the workspace's silently stopped, invisible to an
  audit that cannot enumerate doctests. Doc-only sweeps sit outside the
  coverage pair audit (reported on the coverage line) and are execution
  policy, never build shape.
- `tests` / `skip` / `only` (optional, default `[]`) - per-`[[check]]` libtest
  filters, **ANDed** with any referencing profile's filters of the same name
  (they append, never replace): `tests` -> cargo `--test <name>` target
  selectors, `skip` -> libtest `--skip <substr>`, `only` -> libtest positional
  substring filters. This lets a curated subset (e.g. one named test per
  package) be expressed as several sibling `[[check]]` entries under one
  profile, rather than one sweep running a whole package set's tests.
- `profile` (optional, unset) - the cargo profile this sweep compiles and runs
  under: `"dev"` (alias `"debug"`) or `"release"`. Unset means the command's own
  default, which is what every sweep did before this key existed: dev under
  `brokkr check`, and the CLI / `[test] debug` answer under `brokkr test`.
  Custom `[profile.*]` names are not accepted - brokkr derives
  `BROKKR_TEST_BIN_DIR` from the profile's `target/` subdirectory, and cargo's
  mapping from a custom profile to its directory isn't reliably readable.

  This is the only way to express a **per-sweep profile split**. `[test] debug`
  is one project-wide default and applies to `brokkr test` alone; a profile lane
  can't carry a profile either, because a lane selects *which tests run*, never
  *how they are built*. A repo with a wall-clock contract that only holds
  optimized declares one entry with `profile = "release"` and puts the timing
  tests in it, while the rest of the suite keeps the fast dev build:

  ```toml
  [[check]]
  name = "timing"
  profile = "release"
  only = ["tape_lateness_under_acceleration", "read_market_latency"]
  curated = true

  [test.profiles.timing]
  sweeps = ["timing"]
  ```

  The profile reaches **every** cargo run that compiles the sweep - clippy, the
  test-phase pre-build, the test run, the process-isolated per-test
  invocations, and the coverage enumeration - and `BROKKR_TEST_BIN_DIR` points
  at the matching `target/` subdirectory, so a test that spawns its
  just-rebuilt binary finds the one this sweep actually built.

  It is part of the **build shape**, unlike `curated` and
  `test_exclude_packages`: `cfg(debug_assertions)` decides which code exists, so
  a dev and a release sweep of the same features are two different compiles and
  two different lint surfaces, and neither dedupes into the other. Two
  consequences worth planning for - clippy runs once per profile rather than
  once for both, and under a `certifies = "complete"` profile the release sweep
  enumerates its own coverage universe, so a narrow one wants `curated = true`
  the same way a narrow `rustflags` sweep does.

  `brokkr test` honours it too, one sweep at a time: an explicit
  `--debug`/`--release` still wins, then the sweep's `profile`, then
  `[test] debug`. That is what keeps the split honest - a project can set
  `[test] debug = true` for the fast inner loop without the documented
  `brokkr test tape_lateness_under_acceleration` silently switching to dev and
  failing on the build profile rather than on the code.
- `curated` (optional, default `false`) - declares the entry a hand-picked
  subset for coverage purposes, and requires the entry to carry its own
  `tests`/`skip`/`only` filters (an unfiltered "curated" entry is just an
  uncounted full sweep - parse error). Two effects, both scoped to
  `certifies = "complete"` profiles: the entry is **exempt from the
  complete-universe rule** (a `complete` profile need not reference it, so a
  curated subset can live in its own deliberately-run profile while a gate
  exists), and when a gate lane *does* run it, the coverage audit exempts its
  build shape's non-run pairs from the universe - counted, reported in the
  trailer (like `test_exclude_packages`) and in the `--json` `coverage.curated`
  field, never orphaned, and never credited to a `[[quarantine]]` entry. The
  exemption is keyed on the sweeps, not the shape: a non-curated entry sharing
  a build shape with a curated one keeps that shape fully audited. Coverage
  policy only - never part of the build shape, so a curated and a plain sweep
  of the same shape still dedupe to one clippy run.

A worked example - a deterministic-simulation (madsim) gate as several sibling
sweeps sharing one isolated madsim target dir, each running its own curated
subset, selected together by a `sim` profile:

```toml
[[check]]
name = "sim-common"
packages = ["nautilus-common"]
features = ["simulation"]
rustflags = ["--cfg", "madsim"]

[[check]]
name = "sim-core"
packages = ["nautilus-core"]
features = ["simulation"]
rustflags = ["--cfg", "madsim"]
only = ["virtual_time"]
curated = true

[test.profiles.sim]
sweeps = ["sim-common", "sim-core"]
```

Here `sim-common` runs its package's full test set, so under a complete gate
it must join the gate's lanes like any other entry; `sim-core` runs only a
`virtual_time` subset and is `curated`, so its remaining pairs are exempt and
the `sim` profile stays legal alongside the gate whether or not the gate's
lanes reference it.

The legacy `[check]` table form (with `consumer_features`) is rejected at
parse time with a migration message - move the flags into a `[[check]]` entry.

## `parallel` - running a sweep's test binaries at once

`parallel = { budget = N }` on a `[[check]]` entry runs that sweep's test
binaries concurrently instead of letting cargo run them one after another.

The floor it dissolves: cargo runs each test binary sequentially, and
`--test-threads` parallelizes only *within* a binary. A sweep's wall time
therefore cannot fall below the sum, over binaries, of each binary's slowest
test - and no amount of `test_threads` moves it, which is why a project that
has already tuned threads measures 8 and 16 identically. A single-test binary
contributes its whole duration because it has nobody to overlap with. Running
the binaries concurrently collapses that sum to a maximum.

**The fan-out executes the prebuilt binaries directly** - one prebuild, then
each test executable runs under brokkr's reconstruction of cargo's launch
contract (package-root cwd, loader path, `[env]` config, build-script
`rustc-env`, `CARGO_PKG_*`, runtime `CARGO_BIN_EXE_*` - not `OUT_DIR`, which
cargo gives build scripts and rustdoc, never a test; see `brokkr man check` on direct execution). No
cargo re-entry means the lane is sound on every selection shape,
`test_exclude_packages` included, and touches cargo's build lock only once. Two
consequences: a configured target runner (`[target.<triple>].runner`) or build
target refuses the lane rather than being silently bypassed, and forwarded cargo args that only cargo-mediated execution can
honour (`--no-run`, `--target`, `--config`, `--manifest-path`,
`--target-dir`) are rejected on a parallel sweep.

**The budget counts tests in flight, not binaries.** Binaries times threads is
the real concurrency: seven binaries at `test_threads = 8` is fifty-six
concurrent tests, which on an ordinary box is *slower* than the figure the lane
is chasing. A key naming concurrent binaries would let a config ask for that
while looking like it asked for seven. The number a project has already tuned
is the total, so the total is what the key takes.

**Each binary's slice is proportional to how long it takes**, floored at one
slot and capped at its own test count, handed out largest-first so the long
pole is admitted while the pool is whole. Proportional is not a heuristic:
model a binary of cost `c` on `k` threads as taking `c/k`, and the sweep's wall
time is `max(c_i / k_i)`; that maximum is minimised by holding `c_i / k_i`
equal across binaries, which is exactly threads in proportion to cost. Every
binary then finishes at the same moment, and any other split leaves one binary
a longer pole than it needed to be.

The cost is the binary's **serial cost** - the sum of its own tests'
durations - kept in `.brokkr/parallel-timings.toml` and merged after every
parallel sweep, keyed by the `[[check]]` entry name (never the
lane-qualified label, so `default` under plain check and `tier1/default`
under a lanes gate share one warmth). Test count is only a fallback, because count-as-cost assumes
every test costs the same and starves whichever binary is slow-but-small -
which on a latency-bound suite is precisely the critical path. Measured on the
tree this was built for: a binary of ten integration tests out of 2224 computed
to one slot, and its tests then ran end to end for 22.3s of a 24.5s sweep where
overlapping them would have cost 10.1s.

**Serial cost, and specifically not wall time:** wall time depends on the slots
the allocator granted, so feeding it back oscillates. Serial cost does not move
when the allocation moves.

**Claims are capped where extra threads stop helping.** That same identity
says no binary finishes before its longest single test, so slots past
`serial / slowest` do nothing for it - and are worth more to a binary not yet
at its own floor. The cap is what stops the allocator pouring slots into a
binary that is already as fast as it can be.

> [!NOTE]
> Budget tuning needs settled weights. A fresh tree has no history, so the
> first run weights by count and the second is the first to use measurements.
> Measure a budget change across at least two runs after the store has warmed,
> or a single sample can read as a regression when it is only warm-up.

So the allocation warms up. The first run of a tree has no history and weights
by count exactly as before; a binary with no record of its own is estimated
from its siblings' mean cost per test, and the estimate is replaced by a
measurement as soon as it has run once. The file is advisory in every
direction - missing, unreadable or stale (including one written by an older
brokkr, which no longer parses) costs one warm-up run and never a wrong
result.

Several binaries each claiming the whole budget cannot have overlapped; that
case prints as a warning on an otherwise green run, and the full `claims N-M`
plan line is in the run log (`.brokkr/check-logs/`, see
`docs/commands/check.md`).

`budget = 0` is a load error. There is no spelling for "unlimited": an
unbounded lane is precisely the mistake the key exists to make unspellable.

### The default budget

`parallel = {}` leaves the budget unset, which resolves to **the physical cores
available in one last-level cache domain** - a full CCX/CCD on AMD, the die on
Intel when the process can use that whole domain. The detected cache CPUs are
intersected with the calling thread's affinity (`Cpus_allowed_list` in procfs),
so a cpuset or affinity restriction does not count CPUs the process cannot
run on. `brokkr env` prints the resulting number on its `l3 domain:` line.

Deliberately not logical CPUs. Two tests on the SMT siblings of one core
contend for that core's execution resources, and on a chiplet part two tests on
separate dies pay fabric latency on every shared cache line. Both look like
parallelism without delivering it, so a default taken from `nproc` would spend
wall time in a lane that exists to save it.

This default suits a CPU-bound suite. A suite that is **latency**-bound -
integration tests that spawn subprocesses, wait on daemons, or sleep - profits
from oversubscription well past the core count, because its threads are mostly
idle. One measured tree ran 3x faster at 24 in flight than at 8 on a machine
with fewer cores than either. If a sweep's tests spend their time waiting
rather than computing, set `budget` explicitly and measure; the cache-domain
default is a floor for such a suite, not an estimate of it.

There is also an arithmetic floor the default can sit under, and its failure
reports itself as a clean green run. The pole binary's proportional claim is
`budget x pole_serial / total_serial`, floored at one slot - so until the
budget clears `total_serial / pole_serial`, the pole can never claim a second
slot, its tests run end to end, and the sweep converges to `claims 1-1` at
roughly the pole's full serial cost. Measured on a ~150-binary suite: budget 6
put the 193s pole at a 0.94-slot share, both warm runs landed `claims 1-1`,
and the sweep took longer than not fanning out at all. No warning catches this
shape - nothing in one run distinguishes a starved pole from a pole that is
simply the floor - so on a many-binary suite, read the `claims N-M` plan line
in the run log for the first warm runs and raise the budget past
`total_serial / pole_serial` (both figures are in `--timings`) before judging
the lane.

On AMD the domain is a CCX/CCD; on Intel it is usually the whole die, and
hybrid parts count P-cores and E-cores alike.

### Doctests

Doctests live in cargo's `--doc` pseudo-target, which has no binary for a
`parallel` sweep to fan out over, and nextest does not execute them. When
`[test] doctests = true`, load rejects a config whose `[[check]]` entries are
all parallel or nextest lanes, because none can run doctests. A complete
profile must also select at least one lane that actually runs them; this is
checked per profile, since partial profiles do not claim complete coverage.

A plain libtest lane can carry doctests when its package and target selection
includes them. A `doc_only = true` entry is the explicit alternative: it runs
`cargo test --doc` with its own selection and can provide the workspace-wide
doctest carrier for a parallel profile. Both obligations are load errors, so
the configured complete claim cannot pass without its declared doctests.

When a parallel sweep is workspace-wide but the serial remainder is
package-scoped, those serial siblings only run their packages' doctests. Add a
workspace-shaped `doc_only = true` twin of the parallel entry to carry doctests
for the rest of the workspace; see the `doc_only` key above for the selection
rule a complete profile enforces.

### What it does not do

It does **not** isolate. Tests within one binary still share a process, and
that is the point - a shared-process parallel lane is the only place the
process-global-state class (two tests contending over a global logger and a
shared capture buffer) is visible at all. `isolation = "process"` dissolves
that contention along with the bug's visibility, and the two keys are mutually
exclusive: a sweep setting both is refused before any test runs.

What it *adds* is exposure to **machine-global** state. Several binaries at
once is several processes at once, so a per-machine singleton - a daemon
holding an instance lock, a fixed socket path, a shared state dir - becomes
contended where a sequential lane never showed it. That class arrives as
**flakes rather than clean failures**, which is the thing most likely to get
the lane blamed for a defect it merely exposed. Establish how such state
isolates across concurrent binaries before adopting, and treat the serial side
as the safety valve rather than the tuning knob.

### Enumerate the parallel side, not the serial side

There is deliberately no per-entry serial-group key, because the sweep list
already composes: `[[check]]` entries run strictly one after another, so a
second entry with no `parallel` key **is** the serial lane.

Write the partition with the parallel side enumerated and the complement
unfiltered, never the reverse:

```toml
[[check]]
name = "fanout"
parallel = { budget = 8 }
tests = ["parser_*", "codec_*"]   # opt in, explicitly

[[check]]
name = "rest"                      # unfiltered: today's sequential behaviour
```

Both halves then fail safe. A binary added later lands in the unfiltered
complement and shows up **serial and slow** rather than parallel and flaky. A
*stale* name in the enumerated `tests` list is a hard cargo error (`no test
target named X`, globs included), so the parallel side cannot quietly shrink -
note that the dead-filter guard covers `skip` and `only` only, and does not
judge `tests`, so cargo's own target resolution is what is load-bearing here.
It fires on every run rather than only under `certifies = "complete"`, which
makes the coverage gate a third line of defence rather than the first.

Both entries carry the same features, so they share a build shape and there is
no second compile; the coverage ledger already reconciles multi-sweep selections.

## `[test]` section

Optional. Project-wide test settings - a default cargo package, a default
validation profile, a gate profile, the doctest and build-concurrency toggles -
and the named profiles that selectively reference `[[check]]` entries.

```toml
[test]
default_package = "pbfhogg"
default_profile = "tier1"
gate_profile = "gate"
doctests = false

[test.profiles.tier1]
description = "Fast edit loop used by brokkr check (tier 1)"
sweeps = ["all", "consumer"]
skip = ["tier2::", "tier3::", "platform::", "serial::"]
include_ignored = false

[test.profiles.full]
sweeps = ["all"]
include_ignored = true
```

- `default_package` is the cargo package `brokkr test` passes to
  `cargo test -p` when no `-p/--package` is given. Resolution order:
  explicit CLI `-p` > `[test].default_package` > `Project::cli_package()` >
  error.
- `debug` (default `false`) flips `brokkr test`'s cargo profile from release
  to dev. Use it when the project's tests aren't profile-sensitive and the
  faster compile is worth more than the faster run. CLI overrides win:
  `--debug` forces dev, `--release` forces release; the field only decides
  when neither is passed. It is the project-wide **default**, not a rule: a
  `[[check]]` entry's own `profile` sits between it and the CLI flags, so a
  sweep that must run optimized still does under `brokkr test`. Affects
  `brokkr check` not at all - check's test phase builds dev unless the sweep's
  `profile` says otherwise, and this field is never consulted there.
- `doctests` (default `false`) decides whether `brokkr check`'s test phase runs
  doctests. Off by default because CI runs under cargo-nextest, which never
  executes doctests - running them here would gate on a signal CI can't see. In
  the default state each sweep's `cargo test` is scoped to `--tests` (no
  doctests) unless it already names a target (`--test <name>`). Set `true` to
  restore the full `cargo test` default. Project-wide only (no per-`[[check]]`
  or CLI override); `brokkr test <name>` is unaffected. See
  `docs/commands/check.test-lanes.md`.
- `serial_builds` (default `false`) runs every cargo invocation of `brokkr
  check`'s clippy, rustdoc and prepare-build steps one after another. By
  default they run as one chain per target directory (the shared one, each
  isolated `unify-*`/`rustflags-*` dir), the chains concurrent. A target dir
  separates everything cargo writes, but not what a build script writes
  outside its `OUT_DIR` - into the source tree or a shared directory - so a
  project whose build scripts do that sets this. A cargo build dir configured
  anywhere (`CARGO_BUILD_BUILD_DIR`, `build.build-dir`) holds intermediate
  artifacts across target dirs and forces one chain on its own. If Cargo
  config cannot be fully inspected, brokkr also uses one chain; Cargo reports
  unreadable or invalid config files when the build runs.
- `default_profile` is the validation profile `brokkr check` uses when no
  `--profile` is passed. With no profile config, `brokkr check` runs every
  `[[check]]` entry without libtest filters; with no `[[check]]` either, it
  falls back to a single `--all-features` sweep.
- `gate_profile` names the profile `brokkr check --gate` runs. Load-time
  validation requires it to exist and to carry `certifies = "complete"`, so
  docs and hooks can say `brokkr check --gate` and stay correct through
  profile renames.
- `[test.profiles.<name>]` declares a test selection layered onto one or more
  `[[check]]` entries. Fields: `sweeps` (required, list of `[[check]]` entry
  names), `tests` (`--test <name>`), `only` (positional substring filter),
  `skip`, `include_ignored`, `test_threads`, `env`. `extends = "<other>"`
  walks the chain with cycle detection; collections are replaced (child
  wins), env merges key-by-key.
- `skip` entries are either bare substrings (libtest `--skip`) or
  package-qualified tables - `{ package = "nautilus-infrastructure",
  pattern = "serial_tests::" }` - filtered out of the enumerated set rather
  than expressed as cargo selection, which is the only way to distinguish
  identical module paths in different packages (integration-test paths carry
  no crate prefix). Qualified entries require `isolation = "process"`
  (enforced at resolve time), and a test name existing in both a skipped and
  an unskipped package is a reported collision, never half-obeyed.
- **`skip` and `only` substrings must be at least four characters**, in a
  profile block or a `[[check]]` entry alike (for a qualified skip, the
  floor is on the `pattern` half). Shorter is a load-time error: a
  substring that short matches nearly every test name, so it suppresses or
  selects tests nobody chose while still matching *something* - which is
  the one dead-filter shape the `coverage` phase cannot detect. Breaking
  change for configs that carried one; the fix is the substring that was
  meant, and there is no opt-in for a broad filter.
- **A filter that matches no test fails `brokkr check`.** Under a
  `certifies = "complete"` profile the `coverage` phase asserts every
  `skip` and every `only` - profile-level and entry-level alike, each
  individually - against the enumeration, and reports a dead one with the
  block it was written in. A filter is judged against the sweeps *it*
  applies to: an entry's filter against the lanes running that entry, a
  profile's filter against every sweep the profile runs. So a profile-level
  skip naming a test outside a package-scoped sweep is live as long as some
  sweep of that profile runs it. Same rule as a stale `[[quarantine]]` entry, and
  for the same reason: a filter selecting nothing is a name that drifted,
  and a dead `only` leaves its lane evaluating nothing while still reading
  as a gate. Complementary to the test phase's `zero tests ran` refusal,
  which catches the same defect at run time in any sweep that runs. See
  `docs/commands/check.test-lanes.md`.
- `isolation = "process"` runs each of the profile's tests in its own
  `cargo test ... -- --exact <name>` process.
  `--test-threads=1` serializes tests inside one process per test binary; it
  does not isolate them, and tests touching process-global state (a global
  logger) need the fresh-process guarantee CI's nextest provides. The sweep's
  selection argv is reused verbatim per test - identical build fingerprint,
  cargo-provided test env - at the cost of one cargo spawn per test, sized
  for a serial family of a dozen tests, not thousands. Requires
  `test_threads` unset or 1; never runs doctests; `brokkr check -- ...` extra
  args are rejected on an isolated sweep. Merges through `extends`.
- **`test_threads` other than 1 under a `complete` claim** is a load-time
  error for any plain libtest `[[check]]` entry the profile runs (directly or
  through a lane): cargo-mediated parallelism shares one output stream, so
  its executions cannot be attributed. The error names the profile and the
  entries; give them `parallel = { budget = N }`, or move them to a profile
  without `test_threads`. Parallel, nextest and doc-only entries are exempt.
- `lanes = ["tier1", "serial"]` composes profiles as a *list of runs*:
  each lane resolves independently and the test
  phase runs every lane's sweeps in order (labels lane-qualified,
  `tier1/default`), while the clippy phase dedupes on build shape so two
  lanes sharing a `[[check]]` entry are linted once. The opposite of
  `extends`: tier1's `skip` and serial's `only` are contradictory by
  construction, so no merge can express them. A lanes profile may carry only
  `lanes`, `certifies`, `skip_phases`, and `description`; its lanes must
  exist, must not nest, and must not declare `certifies` themselves (the
  claim belongs to the composing profile) - all load-time errors. Under
  `certifies = "complete"`, every lane must individually satisfy the interim
  no-narrowing rule. `brokkr test` under a lanes profile keeps one sweep per
  build shape, since it drops filters anyway.
- `certifies = "complete" | "partial"` declares what a green run of the
  profile claims, and permissions derive from the claim.
  `partial` may set `skip_phases` (subtractive list of check phases, validated
  against the phase names) and use `-p` scoping; it prints `check partial` and
  exits **10** on success so `brokkr check && git commit` fails closed.
  `complete` prints `check complete`, exits 0, rejects `skip_phases`, `-p`,
  and `extends` (an inherited filter set defeats an explicit claim), and runs
  the **coverage phase**: every (build shape, test) pair that no lane runs
  must be justified by a `[[quarantine]]` entry or be `#[ignore]`d at the
  source, or the check fails as orphaned. `[test] doctests = false` needs a
  `[[quarantine]] category = "doctests"` entry. Profiles without `certifies`
  behave exactly as before the key existed (binary exit codes, `check
  passed`). Neither `certifies` nor `skip_phases` is inherited through
  `extends`.
- Profiles use Rust module paths as the annotation surface; `only` / `skip`
  translate directly into cargo substring filters and `--skip`.
- The legacy `[test.sweeps.*]` map is rejected at parse time. Sweeps now live
  in `[[check]]` entries; profiles reference them by name.

## `[[quarantine]]` entries

The justification ledger for coverage accounting, audited on every `certifies = "complete"` run:

```toml
[[quarantine]]
pattern = "test_twap_calculates_size_schedule_with_remainder"
issue   = "B14"
reason  = "expectation width-sensitive under unified 128-bit build"

[[quarantine]]
category = "doctests"
issue    = "B42"
reason   = "42/55 persistence doctests fail to compile"
```

- Exactly one of `pattern` (a test-name substring, libtest filter
  semantics) or `category` (only `"doctests"`). `issue` and `reason` are
  required - the issue ID is what turns the list from a graveyard with
  good manners into a countdown.
- `package = "<pkg>"` (optional, `pattern` entries only) restricts the
  entry to one package's pairs. Without it, a name-only pattern absorbs
  same-named pairs in every package, so a test that later stops running
  for an unrelated reason lands as accounted instead of orphaned.
- Staleness is mechanical in both directions: a `pattern` entry justifying
  zero non-run pairs fails the check (delete it when the bug closes), and
  a `doctests` entry with `[test] doctests = true` is rejected at load
  time. Per-entry pair counts print on every complete run, so an entry
  silently growing (a new test riding an old substring) is visible.

For the sweep-selection ladder used by `brokkr check` (and how `brokkr test`
diverges) see `docs/commands/check.md`.
