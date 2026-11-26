# Sweeps and test profiles

This page covers the brokkr.toml tables that decide what `brokkr check` and
`brokkr test` build and run. The check section covers `[[check]]` sweeps, the
lint_sweep section covers shapes that are linted and never tested, and the
parallel section covers the concurrent test lane. The doctests section covers
the doctest obligation, the test section covers `[test]` and its profiles,
and the quarantine section covers the coverage ledger. The rest of the schema
is in `brokkr man config`; how the lanes run is in `brokkr man test-lanes`.

## check

The optional `[[check]]` array declares sweeps. Each entry is one clippy and
test sweep with the entry's feature flags, and profiles in `[test.profiles]`
reference entries by name. A shape you want linted but can't or needn't test
is a `[[lint_sweep]]` instead (see lint_sweep below).

These entries declare a full sweep, a consumer sweep, and a sweep in a
virtual workspace:

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

# Virtual workspace: `packages` makes `--features` legal,
# and `env` pins a build-affecting var for the whole sweep.
[[check]]
name = "core"
packages = ["nautilus-core", "nautilus-common"]
features = ["high-precision"]
env = { HIGH_PRECISION = "1" }
```

The legacy `[check]` table form, with `consumer_features`, is rejected at
parse time with a migration message. Move the flags into a `[[check]]` entry.

The keys with short definitions are listed here; the longer ones have their
own sections below:

- **`name`** (required): the label shown in output and the key profiles use
  to reference the entry. It must be unique.
- **`features`** (default `[]`): an explicit list of cargo features. The
  `features = "all"` sentinel is rejected. Enumerate features so that adding
  one to `Cargo.toml` doesn't silently broaden the sweep. See features below
  for package-qualified tokens and narrowed runs.
- **`no_default_features`** (default `false`): emits
  `--no-default-features`.
- **`build_packages`** (default `[]`): cargo packages rebuilt with the
  entry's feature flags, projected onto each package, before the test phase.
  You need it when `tests/cli_*.rs` integration tests invoke a separate CLI
  workspace member. Without it, `cargo test -p LIB` leaves that binary as it
  was last built, and the consumer-sweep contract goes unverified.
- **`packages`** (default `[]`): the packages the sweep is scoped to,
  emitted as `-p PKG` on both `cargo clippy` and `cargo test`. A virtual
  workspace (one with no root package) needs it to use `features`, since
  cargo rejects `--features` at the workspace root. `build_packages` only
  pre-builds CLI binaries; `packages` scopes the check itself.
- **`test_exclude_packages`** (default `[]`): packages omitted from the test
  phase only, emitted as `cargo test --workspace --exclude PKG`. Clippy still
  runs workspace-wide. Use it for a member whose test binary can't link here,
  for example one needing a system library the host lacks. It is mutually
  exclusive with `packages`, since you can't both `-p`-select and
  `--workspace`-exclude; setting both is a parse error.
- **`env`** (default `{}`): environment variables exported to every cargo
  subprocess the sweep runs: clippy, the test-phase pre-build and the test
  run. Use it to pin a build-affecting toggle, such as a codegen flag whose
  drift you'd otherwise see only in `git status`, so `brokkr check` is
  reproducible without exporting it by hand. It merges under a referencing
  profile's `env`, and the entry wins on a key collision.
- **`tests`, `skip`, `only`** (default `[]`): per-entry libtest filters.
  `tests` becomes cargo `--test NAME` target selectors, `skip` becomes
  libtest `--skip SUBSTR`, and `only` becomes libtest positional substring
  filters. They are ANDed with a referencing profile's filters of the same
  name: they append and never replace. You can express a curated subset, such
  as one named test per package, as several sibling entries under one
  profile instead of one sweep running a whole package set's tests.
- **`lib_tests`** (default `false`): selects the library unit-test harness,
  cargo `--lib`. Cargo unions it with the `tests` selectors as it unions
  target flags, so `lib_tests = true` with `tests = ["error_boundary"]` runs
  the lib's unit tests plus that one integration test. Cargo's
  explicit-selection rule applies: every selected package's library is
  selected, `test = false` or not. It is also a profile key, where an
  explicit value replaces an inherited one (`false` clears it). It isn't
  spelled `tests = ["lib"]`, which names an integration test called `lib`.
- **`profile_required`** (default `false`): see profile below.

### features

A `features` entry may be package-qualified (`piners-vm/opcode-counts`), and
every entry is written against the entry's own selection. Some runs select
fewer packages than the entry:

- `brokkr check -p`
- `brokkr test`'s one package
- each resolution of `feature_unification = "package"`
- each `build_packages` pre-build

Such a run drops, and announces, exactly the tokens that only packages
outside its concrete selection route. Every other token is kept and reaches
cargo as written:

- unrecognized tokens, which nothing in the entry's selection routes
- malformed tokens (`dep:x`, `a/b/c`)
- `dep/feature` requests whose dependency a selected package declares, even
  when that dependency lacks the feature

Cargo rejects the last kind with its own message, as the full run would. The
routing rule is in `brokkr man check feature-projection`.

### rustflags

The `rustflags` key (default `[]`) is a token list of extra `rustc` flags,
such as `["--cfg", "madsim"]`. brokkr exports them as `RUSTFLAGS` on the
sweep's cargo processes only, appended to any inherited `RUSTFLAGS`. When the
environment already has the encoded form, brokkr composes with
`CARGO_ENCODED_RUSTFLAGS` instead.

A non-empty value auto-isolates the sweep into its own target dir,
`target/rustflags-HASH`, keyed by the flag content. A global cfg like
`--cfg madsim` reshapes every fingerprint in the build graph, so sharing the
default `target/` would force a full recompile in both directions each time
the sweep alternates with a plain one. Sweeps with identical flags share one
dir, so several madsim legs compile the simulator once. There is no key to
set: isolation is automatic and derived from the flags.

Setting `RUSTFLAGS` or `CARGO_TARGET_DIR` in `env` alongside `rustflags` is a
parse error, so each has one unambiguous source. An argument holding a space,
such as `--check-cfg=cfg(mode, values("a"))`, is kept whole: brokkr moves the
value to `CARGO_ENCODED_RUSTFLAGS` and decodes the inherited `RUSTFLAGS` into
it.

Cargo never hands `RUSTFLAGS` to rustdoc, so brokkr also places the `--cfg`
and `--check-cfg` entries, and only those, in rustdocflags wherever the
sweep's rustdoc runs:

- the rustdoc phase's `cargo doc`
- every `cargo test` of the sweep's test lanes, so its doctests (inline,
  `doc_only`, or a derived carrier) are collected and compiled under the cfg
  their library was built with

brokkr puts them where cargo reads them without displacing the project's own
rustdocflags. It appends them to `CARGO_ENCODED_RUSTDOCFLAGS` or
`RUSTDOCFLAGS` when either is set, the sweep's `env` included. Otherwise it
passes `--config` at the live `target."cfg(all())".rustdocflags` or
`build.rustdocflags` layer. A unit that cargo gives no rustflags gets none of
these either, such as a proc-macro crate under a `target` sweep.

One case is unsupported. When a `target.*.rustdocflags` table in the cargo
config has a selector brokkr can't evaluate, the flags go in
`build.rustdocflags` on a guess, and the run warns once per sweep. If that
table applies, rustdoc runs without the cfg. Setting `RUSTDOCFLAGS` in the
sweep's `env` makes the placement exact.

### feature_unification

The `feature_unification` key (default `"auto"`) picks the feature graph the
sweep's cargo invocations resolve against: `"auto"`, `"selected"`,
`"workspace"` or `"package"`. The three explicit values mirror cargo's own
`resolver.feature-unification` vocabulary. brokkr pins them on the argv with
`-Zfeature-unification` plus a `--config`, which outranks a repo's committed
`.cargo/config.toml`, so a lane's resolution never depends on an ambient
config line.

`"auto"` is brokkr policy, not a cargo mode, and it means the same as leaving
the key out. It pins nothing, except that the parallel lane still promotes an
eligible whole-workspace fan-out to `workspace`.

`"package"` is the `cargo install` boundary and the only mode that catches a
crate depending on features a sibling member donates. It requires a
non-empty `packages` and resolves one cargo invocation per package. It
auto-isolates the sweep into `target/unify-package` (with a `-HASH` suffix
for a sweep with `rustflags`), for the same fingerprint reason as
`rustflags`.

brokkr honours an explicit `"selected"` everywhere, parallel whole-workspace
sweeps included. The fan-out executes the prebuilt binaries directly, so the
prebuild's resolution is the run's resolution. More is in
`brokkr man test-lanes feature-unification`.

### harness

The `harness` key (default `"libtest"`) picks the engine that executes the
sweep's test phase:

- **`"libtest"`**: brokkr's own lanes (serial, `test_threads`, `parallel`,
  `isolation = "process"`).
- **`"nextest"`**: the linked nextest engine, as a fast process-per-test
  executor.

brokkr synthesizes the nextest config per run: zero retries, no
default-filter, no test groups, and brokkr's own 20s per-test watchdog as
`slow-timeout` plus `terminate-after`. brokkr never opens the checkout's
`.config/nextest.toml` and never reads `NEXTEST_PROFILE`, so everything the
sweep runs and claims comes from `brokkr.toml`.

brokkr owns the compile shape: selection, features, unification pin, profile
and `[lints]` allows. It translates the sweep's `skip` and `only` into the
engine's own filters, and maps the profile's `test_threads` onto the
engine's in-flight count (unset means the number of CPUs). `harness` is
execution policy and never part of the build shape, so a nextest and a
libtest sweep of equal compile inputs share the target dir and dedupe in
clippy. Use it for a sweep that needs process isolation, run concurrently
instead of as one cargo spawn per test.

A nextest sweep is legal under a `certifies = "complete"` profile. The
`prepare` phase takes a nextest lane's selection from the engine's own
listing, the one the lane then executes, keyed per binary like every other
lane. A package-scoped `[[quarantine]]` entry spans every binary of its
package.

These combinations are load errors:

- `harness = "nextest"` with `parallel`
- `harness = "nextest"` with `feature_unification = "package"`
- `harness = "nextest"` on a profile setting `isolation = "process"`, which
  is redundant since the engine already isolates

More is in `brokkr man test-lanes nextest`.

### doc_only

The `doc_only` key (default `false`) makes the entry run `cargo test --doc`,
doctests and nothing else. It runs serially, with the entry's selection,
features, unification pin and cargo profile. Doctests live in cargo's `--doc`
pseudo-target, which no binary fan-out can run. So outside a complete claim,
a doc-only entry is how you run the doctests of a `parallel` or nextest
build.

A doc-only entry runs doctests regardless of `[test] doctests`, since the key
is the entry's own opt-in. It uses cargo's meaning of `--doc`, which also
tests a library whose manifest sets `doctest = false`. Under a complete claim
it discharges the doctest obligation of the tested build it equals. The claim
derives a carrier for every other tested build, so the entry is never
required there (see doctests below).

`--doc` is exclusive, so a doc-only entry owns cargo target selection
outright. brokkr refuses these on it:

- entry `tests`, `lib_tests` and `exclude_tests`
- forwarded target selectors
- entry-level `skip` and `only`, since doctests can't be enumerated and a
  filter on them could never be audited for liveness
- under a complete claim, profile-inherited filters too; compose the doc
  twin as its own filterless lane

Doc-only sweeps sit outside the coverage pair audit, which reports them on
the coverage line. They are execution policy, never build shape.

### exclude_tests

The `exclude_tests` key (default `[]`) lists integration-test targets, by
name or by glob using `*` and `?`, that the sweep doesn't run. It is unioned
with any profile `exclude_tests`, and it is never cargo argv. Cargo has no
complement selector, and writing the complement out as explicit `--test`
flags would change what cargo builds and refuses: a bin whose required
features are off, or a `--lib` no package can scope.

The targets are built with the rest and dropped from what the lane executes.
So only a lane that runs its test binaries itself can honour the key:

- `parallel`
- `harness = "nextest"`, as a filterset term
- a profile's `isolation = "process"`

A serial lane refuses it when its sweeps resolve. An entry matching no
integration-test target the lane built is an error, the stale-name failure
cargo gives `tests`. Excluded targets' tests are non-run pairs for the
coverage audit, so `curated` accepts the key. Under a complete claim the
universe enumeration still lists every built harness, an excluded one
included: exclusion stops its tests running, not its listing. brokkr refuses
`lib_tests` and `exclude_tests` with `doc_only`.

### profile

The `profile` key (unset by default) is the cargo profile the sweep compiles
and runs under: `"dev"` (alias `"debug"`) or `"release"`. Unset means the
command's own default: dev under `brokkr check`, and the CLI or
`[test] debug` answer under `brokkr test`. Custom `[profile.*]` names aren't
accepted. brokkr derives `BROKKR_TEST_BIN_DIR` from the profile's `target/`
subdirectory, and cargo's mapping from a custom profile to its directory
isn't reliably readable.

This key is the only way to split profiles per sweep. `[test] debug` is one
project-wide default and applies to `brokkr test` alone. A profile lane can't
set a cargo profile either, because a lane selects which tests run, never how
they are built. A repo with a wall-clock contract that only holds optimized
declares one release entry with the timing tests, and the rest of the suite
keeps the fast dev build:

```toml
[[check]]
name = "timing"
profile = "release"
only = ["tape_lateness_under_acceleration", "read_market_latency"]
curated = true

[test.profiles.timing]
sweeps = ["timing"]
```

The profile reaches every cargo run that compiles the sweep: clippy, the
test-phase pre-build, the test run, the process-isolated per-test
invocations, and the coverage enumeration. `BROKKR_TEST_BIN_DIR` points at
the matching `target/` subdirectory, so a test that spawns its just-rebuilt
binary finds the one this sweep built.

Unlike `curated` and `test_exclude_packages`, the profile is part of the
build shape. `cfg(debug_assertions)` decides which code exists, so a dev and
a release sweep of the same features are two compiles and two lint surfaces,
and neither dedupes into the other. Plan for two consequences:

- Clippy runs once per profile, not once for both.
- Under a `certifies = "complete"` profile, the release sweep enumerates its
  own coverage universe, so a narrow one wants `curated = true`, as a narrow
  `rustflags` sweep does.

`brokkr test` honours the key too, one sweep at a time. An explicit `--debug`
or `--release` wins, then the sweep's `profile`, then `[test] debug`. That
order keeps the split honest. A project can set `[test] debug = true` for the
fast inner loop, and the documented
`brokkr test tape_lateness_under_acceleration` still won't switch to dev and
fail on the build profile instead of on the code.

The `profile_required` key (default `false`) says the sweep can only run
under its `profile`, which it then requires; brokkr refuses the entry without
one. Use it for a sweep whose features only compile with `debug_assertions`
(a `compile_error!` under `not(debug_assertions)`), or whose tests assert
what only one profile does.

`profile_required` changes `brokkr test` alone, the one command with a
profile flag. A `--debug` or `--release` asking for the other profile leaves
the sweep out of the run before anything is built, instead of overriding the
pin into a guaranteed `BUILD FAILED`. brokkr names it on the plan line
(`requires dev: SWEEP`) or, for a lone sweep, on a `SKIP` line. With no flag,
or the agreeing one, nothing changes. The key isn't part of the build shape,
since it decides which runs include the sweep and never what is compiled.
`[[lint_sweep]]` doesn't accept it, since nothing overrides a lint sweep's
profile. This entry pins a dev-only fault-injection sweep:

```toml
[[check]]
name = "fault-injection"
features = ["fault-injection"]
profile = "dev"
profile_required = true
```

### target

The `target` key (unset by default) is the target triple the sweep compiles
and runs for, as a bare name such as `"x86_64-unknown-linux-musl"`. Custom
`.json` specs are refused, and unset builds for the host. Use it for code
gated on `cfg(target_env)` and friends, which no host sweep compiles:

```toml
[[check]]
name = "musl"
target = "x86_64-unknown-linux-musl"
build_packages = ["pbfhogg-cli"]
parallel = {}
```

Like `profile`, the triple reaches every cargo run of the sweep as
`--target`: clippy, rustdoc, the pre-builds, the test run, process-isolated
runs, nextest and the coverage enumeration. It is build shape, so a host and
a musl sweep of the same features never dedupe in clippy and keep separate
coverage universes. `BROKKR_TEST_BIN_DIR` points at
`TARGET_DIR/TRIPLE/PROFILE`.

A triple may open with `{host_arch}` in place of its architecture, naming
the host's own member of a target family:

```toml
[[check]]
name = "musl"
target = "{host_arch}-unknown-linux-musl"
```

`{host_arch}` is the architecture component of the host triple, from the
`host:` line of `rustc -vV`. The compiler is the one cargo would use under
the sweep's env, the same one the preflight resolves against (see target
preflight below). On an x86_64 host the sweep above is
`x86_64-unknown-linux-musl`, and on an aarch64 host it is
`aarch64-unknown-linux-musl`. The component is the triple's first one as
spelled, so an armv7 host yields `armv7-unknown-linux-musl`, which is no
target; spell such a family literally.

Config load checks the spelling only. Each run resolves `{host_arch}` before
anything runs, and from then on the sweep is the concrete triple: the build
shape, the cargo argv, `check --audit` and every phase see it, so it dedupes
with a literal entry naming the same triple. A compiler that can't report its
host fails the run, naming the sweep. `{host_arch}` works the same in a
`[[lint_sweep]]`.

### Target execution

brokkr executes a target sweep's test binaries itself in every lane, so the
host must be able to run them. x86_64 musl runs natively on an x86_64 glibc
host, so the whole suite runs, parallel lane included.

brokkr learns whether this machine can execute the binaries by executing
them, never in advance. A binary it can't execute fails when it is first run,
with the OS error (`Exec format error`) and the file. No pre-build check
refuses a triple, because nothing built ahead of the lane's own binaries
proves what the kernel does with them. A binfmt_misc handler, or the ELF
loader, accepts or rejects individual files of one triple. And the triple's
facts alone would refuse an emulated host that runs such a sweep today.

Before anything builds, a run does report a difference. For each tested
target lane it attempts, brokkr prints one line when the triple's
`target_os`, `target_arch`, `target_pointer_width` or `target_endian` differ
from the compiler host's. The line names the sweep, the triple and the
compiler host:

```text
sweep 'musl': target aarch64-unknown-linux-musl differs from compiler host
x86_64-unknown-linux-gnu; ...
```

The line states a difference, not a verdict. brokkr prints none when the
facts agree or can't be read, and `brokkr test` prints it too.

brokkr refuses, rather than bypasses, a runner that may apply to the
triple:

- `[target.TRIPLE].runner`
- its `CARGO_TARGET_TRIPLE_RUNNER` spelling
- a `[target.'cfg(...)']` runner brokkr can't prove false for the triple

So emulation through a cargo runner, such as qemu as `runner`, isn't
supported. Emulation the kernel applies itself (binfmt_misc) is invisible to
brokkr and runs like native execution. A host runner keeps its usual
treatment.

### Target preflight

Before anything builds, each test lane resolves the triple against the
compiler cargo would use under the sweep's env: `RUSTC`, `CARGO_BUILD_RUSTC`,
`build.rustc`, then `rustc`. The test lanes' preflight refuses:

- a triple the toolchain has no standard library for
  (`rustup target add TRIPLE`)
- a cargo older than 1.89, which builds a cross target's doctests without
  running them
- forwarded `--target`, `--config`, `--manifest-path` or `--target-dir`

Clippy refuses none of these. The `rustdoc` phase only reads the triple's
cfg facts from the same compiler, to place rustdocflags. So neither refuses
a missing standard library or an older cargo; rustc reports a missing
library itself wherever a crate needs one.

The sweep's `env` may not set `CARGO_BUILD_TARGET`, and brokkr overrides an
ambient one. There is no check for a C compiler. Which one a `cc`-based build
script picks is its own decision, and its error names it (`musl-gcc` from
`musl-tools` on Debian and Ubuntu).

Cargo's own rules apply under `--target`. `RUSTFLAGS` and the `rustflags`
tables reach the target's units only, not build scripts or proc-macros. So
the same goes for a sweep's `rustflags`, the `[lints] allow` flags, and the
cfg entries placed in rustdocflags.

brokkr matches `[target.'cfg(...)']` tables against the triple's facts from
`rustc --print cfg --target`, for the names the triple alone fixes:
`target_arch`, `target_os`, `target_family`, `target_env`, `target_vendor`,
`target_abi`, `target_pointer_width`, `target_endian`, `unix` and `windows`.
Any other name leaves the selector undecided. An undecided selector refuses a
runner, and the allows fall back to `build.rustflags`.

### curated

The `curated` key (default `false`) declares the entry a hand-picked subset
for coverage. The entry must have its own `tests`, `lib_tests`,
`exclude_tests`, `skip` or `only` filters; an unfiltered curated entry would
be an uncounted full sweep, so it is a parse error.

Both effects apply only under `certifies = "complete"` profiles:

- The entry is exempt from the complete-universe rule. A complete profile
  need not reference it, so a curated subset can live in its own
  deliberately-run profile while a gate exists.
- When a gate lane does run it, the coverage audit exempts its build shape's
  non-run pairs from the universe. They are counted and reported in the
  trailer, like `test_exclude_packages`, and in the `--json`
  `coverage.curated` field. They are never orphaned and never credited to a
  `[[quarantine]]` entry.

The exemption is keyed on the sweeps, not the shape, so a non-curated entry
sharing a build shape with a curated one keeps that shape fully audited.
`curated` is coverage policy only and never part of the build shape, so a
curated and a plain sweep of the same shape still dedupe to one clippy run.

This config declares a deterministic-simulation (madsim) gate as sibling
sweeps. They share one isolated madsim target dir, each runs its own subset,
and a `sim` profile selects them together:

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

`sim-common` runs its package's full test set, so under a complete gate it
must join the gate's lanes like any other entry. `sim-core` runs only a
`virtual_time` subset and is curated, so its remaining pairs are exempt. The
`sim` profile stays legal beside the gate whether or not the gate's lanes
reference it.

## lint_sweep

The optional `[[lint_sweep]]` array declares build shapes that `brokkr check`
lints with clippy and never tests. With `[rustdoc]`, brokkr also documents
them with rustdoc. A lint sweep is for a shape whose test phase can't run on
this host or isn't wanted. A `[[check]]` entry always runs its tests, and a
cross-target binary the host can't execute fails when it is first run.

This entry lints code behind a `cfg(not(target_arch = "x86_64"))` fallback
from an x86_64 host:

```toml
[[lint_sweep]]
name = "aarch64"
target = "aarch64-unknown-linux-gnu"
features = ["simd"]
```

A lint sweep takes the build-shape keys of `[[check]]`, with the same meaning
and the same load checks: `name` (required), `features`,
`no_default_features`, `packages`, `env`, `rustflags`, `profile`, `target`
and `feature_unification`. It also takes `when` (see when below). These keys
are unknown and a load error:

- every test-shaping key: `tests`, `lib_tests`, `exclude_tests`, `skip`,
  `only`, `curated`, `parallel`, `harness`, `doc_only`, `build_packages` and
  `test_exclude_packages`
- `profile_required`, since no command overrides a lint sweep's profile

The entry is the whole shape. No profile references a lint sweep, and no
profile's `env` reaches it.

`name` must be unique among the lint sweeps and differ from every `[[check]]`
name, since the two share the run's sweep labels and `brokkr clippy --sweep`.
It may not contain `/`, the separator of a lanes profile's `lane/entry`
labels.

### when

The per-entry `when` key decides whether a run includes the lint sweep, by
the run's claim. It uses the vocabulary of `[[script_check]]`,
`[member_check]` and `[[fixture_lint]]`:

| `when` | Runs |
| --- | --- |
| `always` (default) | In every `brokkr check` whose clippy or rustdoc phase runs, under any profile; not in an ad-hoc `--features` run |
| `gate` | Only under a `certifies = "complete"` profile |
| `off` | Never in `brokkr check`; `brokkr clippy --sweep NAME` lints it on demand |

`always` is the default for the reason `[[script_check]]` has it. A lint
sweep is a clippy and rustdoc pass of the kind every check already runs,
while `[member_check]` and `[[fixture_lint]]` compile graphs the edit loop
doesn't, and so start gate-only. A `gate` entry is a load error when no
`[test.profiles.*]` declares `certifies = "complete"`, since it could never
run.

The run log names the entries a run's claim left out, and the console
doesn't:

```text
lint-sweep: 1 gate-only sweep not run, no complete claim: aarch64
```

`brokkr clippy --sweep NAME` runs clippy only. Nothing runs a lint sweep's
rustdoc pass on demand.

### run

A run treats lint sweeps this way:

- The lint lanes join clippy and rustdoc after the tested sweeps. They are
  selected, `-p`-narrowed, deduped and reported by the same rules
  (`brokkr man check invocation-selection`). One whose build equals a tested
  sweep's is deduped onto it (`clippy aarch64: deduped`). So a lint sweep
  costs nothing where a running `[[check]]` entry already compiles its
  shape, and under a profile that doesn't select that entry it is real work.
  A `-p` that its `packages` list rules out entirely excludes it, as it
  excludes a `[[check]]` entry.
- The test machinery never sees them: no preparation, test lane, coverage
  universe, doctest carrier or doc-only rule. The tested sweeps' compile
  inputs, test obligations and preparation scheduling are what they would be
  without the lint sweeps. A derived doctest carrier only skips a label a
  lint sweep holds (`foo (doctests 2)`).
- Clippy lints them with `--all-targets` like every gate sweep, so the triple
  must compile the tests, benches and examples too. Rustdoc documents cargo's
  default doc targets.
- No binary for the triple is linked or run, so the triple itself needs only
  its standard library (`rustup target add TRIPLE`). Build scripts and
  proc-macros are host code and still build and run. One that compiles C for
  the target needs that target's C compiler or system libraries, and the
  entry's `env` can point it at them.
- An entry's `env` may also configure a cargo build dir. That serializes the
  clippy and rustdoc runs and leaves preparation alone.
- A missing standard library for the triple fails in clippy and rustdoc with
  rustc's own error (`can't find crate for core`, naming `rustup target add`)
  wherever a crate needs it. There is no separate preflight. The rustdoc
  phase reads the triple's cfg facts (`rustc --print cfg --target`), which
  needs no installed library, and refuses nothing about the toolchain.
- Lint sweeps are skipped with the phases that run them: a profile's
  `skip_phases` naming `clippy` and `rustdoc`, and the markdown-only
  shortcut. An ad-hoc `--features` or `--no-default-features` run replaces
  sweep selection wholesale and takes none.
- The `--json` trailer's `sweeps` lists each one clippy or rustdoc reached.
  One deduped onto a tested sweep isn't listed separately.

### composition

Lint sweeps compose by the ordinary array rule (`brokkr man config
include`). You replace an included entry by repeating it in full under its
name, and `lint_sweep = []` clears the inherited list.

brokkr claims the key only as an array, so a `[lint_sweep]` table is still a
hostname section. The table and the array can't both stand at that spelling,
so a host of that name moves to `[host.lint_sweep]`. The user-wide config
doesn't accept the key.

## parallel

The `parallel = { budget = N }` key on a `[[check]]` entry runs that sweep's
test binaries concurrently, instead of letting cargo run them one after
another.

Cargo runs each test binary sequentially, and `--test-threads` parallelizes
only within a binary. So a sweep's wall time can't fall below the sum, over
binaries, of each binary's slowest test, and no `test_threads` value moves
it. That is why a project that has already tuned threads measures 8 and 16
identically. A single-test binary contributes its whole duration, because
nothing overlaps it. Running the binaries concurrently collapses that sum to
a maximum.

### execution

The fan-out executes the prebuilt binaries directly. brokkr runs one
prebuild, then each test executable under brokkr's reconstruction of cargo's
launch contract:

- the package-root cwd
- the loader path
- `[env]` config
- build-script `rustc-env`
- `CARGO_PKG_*`
- runtime `CARGO_BIN_EXE_*`

`OUT_DIR` isn't set, since cargo gives it to build scripts and rustdoc, never
to a test. Direct execution is described in `brokkr man check`.

With no cargo re-entry, the lane is sound on every selection shape,
`test_exclude_packages` included, and takes cargo's build lock only once.
Two consequences follow:

- A configured target runner (`[target.TRIPLE].runner`) or an ambient build
  target refuses the lane instead of being silently bypassed. The sweep's
  own `target` key is honoured.
- brokkr rejects, on a parallel sweep, forwarded cargo args that only
  cargo-mediated execution can honour: `--no-run`, `--target`, `--config`,
  `--manifest-path` and `--target-dir`.

### budget

The budget counts tests in flight, not binaries. Binaries times threads is
the real concurrency: seven binaries at `test_threads = 8` is 56 concurrent
tests, which on an ordinary box is slower than the figure the lane is
chasing. A key naming concurrent binaries would let a config ask for that
while looking like it asked for seven. The number you have already tuned is
the total, so the key takes the total.

The key takes a positive integer, `"cores"` (see cores below), or nothing;
any other value is a load error. `budget = 0` is a load error too. There is
no spelling for unlimited, because an unbounded lane is the mistake the key
exists to make unspellable.

The plan line in the run log names how brokkr reached a host-relative
budget: `budget 12 (cores) in flight` or `budget 6 (cache domain) in
flight`. brokkr appends `; no usable topology` when sysfs gave no usable
count, and the number is then `available_parallelism()`, or 4 when that fails
too. It appends `; quota capped from N` when a cgroup quota bound. An
explicit count prints bare.

### allocation

Each binary's slice is proportional to how long it takes, floored at one
slot and capped at the number of tests it executes this run. brokkr admits
binaries in order of claim size, largest first. When the long pole holds the
largest claim, it is admitted while the pool is whole. When it doesn't (a
one-slot pole), it queues behind every wider claim.

Proportional follows from a model. Model a binary of cost `c` on `k` threads
as taking `c/k`; the sweep's wall time is then `max(c_i / k_i)`. That
maximum is smallest when `c_i / k_i` is equal across binaries, which is
threads in proportion to cost. Whole slots, the floor, the count cap and
queued admission make the real allocation an approximation of that optimum.

The cost is the binary's serial cost for this run: the sum of the recorded
durations of the tests it will execute. Those are its selected tests, less
the `#[ignore]`d ones unless the lane runs them, plus the benchmark functions
test mode runs once. brokkr records each test's duration in
`.brokkr/parallel-timings.toml` after every parallel sweep and merges it per
test name. A run doesn't overwrite the record of a test it didn't complete.
Only the 30-day retention under warm-up can remove it. So a plain check that
skips a binary's slow ignored tests leaves their durations in place for the
next gate, and a skip or only filter can't overwrite the costs of tests it
left out. Records are grouped by the `[[check]]` entry name, never the
lane-qualified label, so `default` under plain check and `tier1/default`
under a lanes gate share them. Within an entry a binary is identified by its
cargo resolution, package id and `kind:target`.

Test count is only a fallback. Count-as-cost assumes every test costs the
same, and starves whichever binary is slow but small, which on a
latency-bound suite is the critical path. On the tree the lane was built
for, a binary of ten integration tests out of 2224 computed to one slot. Its
tests then ran end to end for 22.3s of a 24.5s sweep, where overlapping them
would have cost 10.1s.

The cost is serial cost and not wall time, because wall time depends on the
slots the allocator granted, so feeding it back oscillates. Serial cost has
no direct dependence on the grant, though contention between tests under a
wider grant can still move a test's own duration.

There's no cap below the test count. `wall = max(serial / k, slowest)` is
a lower bound on a binary's wall time, not its value, so a cap read off it
can withhold a slot that would have helped. Four tests of 10, 6, 6 and 6
seconds give `serial / slowest` a ceiling of three slots. Three slots take
12s, while four take 10s.

### warm-up

The allocation warms up. The first run of a tree has no history and weights
by count. A test with no record is priced at the mean over the recorded
tests this run executes, across every binary in the plan rather than its
own binary, so one cheap sample can't price its unseen siblings cheap. A
test stays estimated until a run completes it. A failed, aborted or
interrupted run still records the tests that completed. A test it didn't
complete keeps whatever record it had. A binary an interrupt cut off
mid-run reports no tests, so it records nothing.

The timings file never decides which tests are selected or how a result is
judged.
A missing or unreadable file, or one in an older format that no longer
parses, reads as no history. An allocation is still a performance input,
though. A stale estimate can lengthen the test phase, and the phase's time
ceiling judges the longer phase. A test unobserved for 30 days is dropped
on the next write, so renamed and deleted tests don't pile up. That only
limits what the file keeps: until then, an old record is read like a fresh
one.

> [!NOTE]
> Budget tuning needs settled weights. The first run weights by count, and
> the second is the first to use measurements. Measure a budget change
> across at least two runs after the store has warmed, or a single sample
> can read as a regression when it is only warm-up.

When several binaries each claim the whole budget, they can't have
overlapped. brokkr prints that case as a warning on an otherwise green run,
and the full `claims N-M` plan line is in the run log (`.brokkr/check-logs/`,
see `brokkr man check`).

### Default budget

`parallel = {}` leaves the budget unset. It then resolves to the physical
cores available in one last-level cache domain: a full CCX or CCD on AMD, or
the die on Intel when the process can use that whole domain. Hybrid Intel
parts count P-cores and E-cores alike.

brokkr intersects the detected cache CPUs with the calling thread's affinity
(`Cpus_allowed_list` in procfs), so a cpuset or affinity restriction doesn't
count CPUs the process can't run on. `brokkr env` prints this topology count
on its `l3 domain:` line.

brokkr also caps the default budget by the cgroup v2 `cpu.max` quotas in the
calling thread's group and its visible ancestors: quota divided by period,
rounded down, with a minimum of one. Unlimited, unreadable or malformed
quota files add no cap. brokkr can't probe ancestors hidden by a cgroup mount
or namespace. An explicit `budget = N` bypasses the default and its quota
cap.

Neither host-relative budget counts logical CPUs. Two tests on the SMT
siblings of one core contend for that core's execution resources. On a
chiplet part, two tests on separate dies pay fabric latency on every shared
cache line. Both look like parallelism without delivering it, so a default
taken from `nproc` would spend wall time in a lane that exists to save it.

### cores

`parallel = { budget = "cores" }` resolves to every physical core the
process may run on, across all cache domains. brokkr takes the online CPUs in
the calling thread's affinity and collapses them onto their SMT sibling
groups (`topology/core_cpus_list`, or `thread_siblings_list` on older
kernels). It then caps the count by the same cgroup v2 `cpu.max` quotas as
the default. With full affinity, readable topology and no binding quota, a
16-core part gets 16 whatever the chiplet layout, and a 12-core part gets
12.

Use it for a suite the one-domain default starves (see Arithmetic
floor below) that must still follow the host it runs on. A fixed `budget = N` in a
shared `brokkr.toml` can't: one number is right for one machine and
oversubscribes or underuses the others.

The count fails closed. If any allowed CPU has no readable sibling list (a
trimmed sysfs), or two CPUs' sibling lists disagree, brokkr doesn't trust the
topology. The budget then falls back to `available_parallelism()`, which
counts logical CPUs, and the plan line says so. Like the default, `"cores"`
is resolved per run and shown in the log only, so whether a config loads
never depends on it.

Neither budget pins tests to a domain. The default only sizes the lane as if
it ran in one. `"cores"` sizes it for the whole part, and accepts that more
tests in flight means more of them pay cross-die latency, a cost a starved
pole outweighs. A latency-bound suite that wants more than the core count
still needs an explicit number.

There is no per-host budget, since host sections take host options only. A
host that needs a different fixed number can include a machine-local file
holding whole `[[check]]` entries (see `brokkr man config include`). The
entries must move out of the including `brokkr.toml`, because a same-named
entry there wins over the included one and makes the local budget inert.
Every checkout must then have that file, or the config fails to load.

### latency-bound

Both host-relative budgets suit a CPU-bound suite. A latency-bound suite,
whose integration tests spawn subprocesses, wait on daemons or sleep, profits
from oversubscription well past the core count, because its threads are
mostly idle. One measured tree ran 3x faster at 24 in flight than at 8, on a
machine with fewer cores than either. If a sweep's tests spend their time
waiting, set `budget` explicitly and measure. The cache-domain default is a
floor for such a suite, not an estimate of it.

### Arithmetic floor

The default budget can also sit under an arithmetic floor, and that failure
reports itself as a clean green run. The pole binary's proportional claim is
`budget x pole_serial / total_serial`, floored at one slot. Until the budget
clears `total_serial / pole_serial`, the pole can't claim a second slot, its
tests run end to end, and the sweep converges to `claims 1-1` at roughly the
pole's full serial cost.

On a suite of about 150 binaries, budget 6 gave the 193s pole a 0.94-slot
share. Both warm runs reported `claims 1-1`, and the sweep took longer than
not fanning out at all.

No warning catches this shape, since nothing in one run distinguishes a
starved pole from a pole that is the floor. On a many-binary suite, read the
`claims N-M` plan line in the run log for the first warm runs. Raise the
budget past `total_serial / pole_serial` before judging the lane; both
figures are in `--timings`.

### tail

A claim is fixed for a binary's whole run. It is the `--test-threads` the
process started with, and libtest offers no way to widen it. When the small
binaries finish, their slots admit queued binaries and nothing else. Once
the queue is empty, the in-flight count decays to the claims of the binaries
still running.

The equal-finish model in allocation is the plan, and the tail is what a
model of `c/k` can't see: tests of 10s, 9s and 9s take 18s on two threads,
not 14s. A run whose busy count falls well before the end is showing the
tail.

The lane is work-conserving at binary granularity, so the remedy is smaller
binaries:

- **Split the long integration targets at the source.** More `[[test]]`
  targets are more planning units, so a freed slot has queued work to admit.
  Each costs a link, so split the few poles that the `slowest` line and
  `--timings` name, rather than undoing a target consolidation. Keep tests
  that share a provisioned fixture in one target: you choose the boundary,
  which a timing-driven split couldn't know.
- **Use `harness = "nextest"`.** It schedules process-per-test across the
  sweep's binaries under one in-flight count, which is test-granular work
  conservation within the sweep. The price is fail-fast, a process (and its
  fixture setup) per test, and the shared-process detector.

You can't split a library's unit tests into targets this way. A one-slot
library pole is the arithmetic floor, not the tail: its claim was small from
the start.

brokkr doesn't split a binary into several processes itself. The split would
change which tests share a process, so it would need an opt-in key, a
journaled per-process test manifest and per-part validation in accounting.
It still couldn't widen a part once started, and the source split already
gives the gain wherever you can draw the boundary.

### isolation

A parallel lane doesn't isolate. Tests within one binary still share a
process, and that is the point. A shared-process parallel lane is the only
place the process-global-state class of bug is visible at all, such as two
tests contending over a global logger and a shared capture buffer.
`isolation = "process"` dissolves that contention along with the bug's
visibility. The two keys are mutually exclusive, and brokkr refuses a sweep
setting both before any test runs.

What the lane adds is exposure to machine-global state. Several binaries at
once are several processes at once. So a per-machine singleton, such as a
daemon holding an instance lock, a fixed socket path or a shared state dir,
becomes contended where a sequential lane never showed it. That class shows
up as flakes rather than clean failures, which tends to get the lane blamed
for a defect it only exposed. Establish how such state isolates across
concurrent binaries before adopting the lane, and treat the serial side as
the safety valve, not the tuning knob.

### Serial twin

There is deliberately no per-entry serial-group key, because the sweep list
already composes. `[[check]]` entries run strictly one after another, so a
second entry with no `parallel` key is the serial lane.

Write the partition with the parallel side enumerated and the complement
unfiltered, never the reverse:

```toml
[[check]]
name = "fanout"
parallel = { budget = 8 }
tests = ["parser_*", "codec_*"]   # opt in, explicitly

[[check]]
name = "rest"                      # unfiltered: sequential
```

Both halves then fail safe. A binary added later goes to the unfiltered
complement and shows up serial and slow, not parallel and flaky. A stale
name in the enumerated `tests` list is a hard cargo error (`no test target
named X`, globs included), so the parallel side can't quietly shrink.

The dead-filter guard covers `skip` and `only` only and doesn't judge
`tests`, so cargo's own target resolution is what protects this list. Cargo's
error fires on every run, not only under `certifies = "complete"`, which
makes the coverage gate a third line of defence rather than the first.

Both entries have the same features, so they share a build shape and there
is no second compile. The coverage ledger already reconciles multi-sweep
selections.

### folding

Entries run one after another, so a config of several small package-scoped
feature sweeps pays every sweep's tail in series, and the run idles between
them. brokkr doesn't test two entries at once.

Overlap by default would change what an existing multi-entry config means:
the serial twin, the fail-fast order, and the in-flight count of an existing
budget. Even as an opt-in it would need every lane's launch closure kept
apart. Lanes in one target dir overwrite each other's uplifted executables,
and a lane that must rebuild can't compile beside a running test. Overlap
would also let two lanes over one package run the same test beside itself,
over the same `CARGO_TARGET_TMPDIR` and fixture paths, which nothing can
verify. A future pool would have to admit together only lanes that can't run
a test beside itself (disjoint test packages, or disjoint targets of one
package), with separate launch closures.

One entry serves the case instead. Sweeps that each select one different
package with that package's own features fold into a single
`feature_unification = "package"` entry, whose `features` are the union as
bare tokens. Package mode runs one resolution per package, and a bare token
reaches only a resolution whose package defines it (see
`brokkr man check feature-projection`). So each resolution keeps its old
feature set, and the parallel lane queues every resolution's binaries under
one budget:

```toml
[[check]]
name = "feature-sweeps"
packages = ["app-vm", "app-bench"]
feature_unification = "package"
parallel = { budget = 16 }
features = ["opcode-counts", "region-sampling", "hotpath"]
```

The fold changes these things, so treat it as a decision and not a rename:

- The entry lives in `target/unify-package`, a first compile there. The
  plain sweeps stop having their uplifted binaries overwritten by the
  feature builds.
- brokkr computes the claims and queue order over the merged inventory. The
  timing store and coverage labels (`ENTRY/PACKAGE`) start under the new
  name. A formerly serial entry folded in becomes a parallel one.
- The equivalence holds for cargo's default resolution of a one-package
  selection. An ambient `resolver.feature-unification = "workspace"` would
  have changed the old sweeps and doesn't change this one.
- A bare union couples the packages: if one package later adds a feature
  under a name in the list, it is enabled there too.
- Two selected packages emitting a bin of the same name share one path in
  the directory, so keep uplifted names distinct.

## doctests

The doctest obligation decides where a run's doctests execute. Doctests live
in cargo's `--doc` pseudo-target, which has no binary for a `parallel` sweep
to fan out over, and nextest doesn't execute them.

Outside a complete claim, a plain libtest lane runs doctests inline under
`[test] doctests = true` when its target selection includes them. A
`doc_only = true` entry runs them explicitly. When `[test] doctests = true`,
load rejects a config whose `[[check]]` entries are all parallel or nextest
lanes, since an ordinary run would have nowhere to run them.

Under `certifies = "complete"` the obligation is per tested build, whatever
`[test] doctests` says. A doctest under `cfg(feature = "ffi")` compiles only
in a build with that feature, so one workspace doc twin can't stand in for a
package-scoped feature lane.

The claim owes a doctest pass for every lane it runs that is not doc-only or
curated, and whose selection reaches a member with a doctested library. The
pass is `cargo test --doc` of exactly that lane's build, unfiltered and
serial. The build is the lane's requests, features, unification answer,
profile, target, rustflags, env and `build_packages`.

A configured `doc_only` entry of an equal build discharges the pass.
Otherwise the run derives the carrier itself and adds it as a lane,
`ENTRY (doctests)`, announced at the top of the run. brokkr lints nothing
extra for it, since its source lane's own clippy run covers the build.
Equal builds share one carrier, so a parallel lane and its nextest twin owe
one pass. A package-mode lane's carrier runs only the packages that reach a
doctested library.

Serial lanes run no doctests inline under a complete claim. So every doctest
runs once per tested build, and never under a lane's `skip` or `only`. A
derived carrier honours a library manifest's `doctest = false`. Cargo's
`--doc` would test that library anyway, so brokkr declines its rustdoc and
records that in the doctest block. Converting a lane to `parallel` or
`harness = "nextest"` therefore never drops its doctests from the gate.

A `[[quarantine]] category = "doctests"` entry suppresses every derived
carrier, and the run names what it suppresses; configured `doc_only` entries
still run.

A complete claim that owes doctests also needs one workspace-shaped carrier,
configured or derived: `packages` empty, with `test_exclude_packages` allowed
and reported. The derived carriers cover every tested build, but a member no
lane selects is in none of them. Its doctests would stop running unseen,
since the audit can't enumerate doctests. A workspace whose members have no
doctested library owes neither.

## test

The optional `[test]` table holds project-wide test settings: a default cargo
package, a default validation profile, a gate profile, and the doctest and
build-concurrency toggles. It also holds the named profiles, which reference
`[[check]]` entries selectively. This config sets the defaults and declares
two profiles:

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

The `[test]` keys are:

- **`default_package`**: the cargo package `brokkr test` passes to
  `cargo test -p` when you give no `-p` or `--package`. The order is the CLI
  `-p`, then `[test].default_package`, then the project's built-in CLI
  package, then an error.
- **`debug`** (default `false`): flips `brokkr test`'s cargo profile from
  release to dev. Use it when the project's tests aren't profile-sensitive
  and the faster compile is worth more than the faster run. CLI flags win:
  `--debug` forces dev and `--release` forces release, and the key decides
  only when you pass neither. It is the project-wide default, not a rule: a
  `[[check]]` entry's own `profile` sits between it and the CLI flags, so a
  sweep that must run optimized still does under `brokkr test`. It doesn't
  affect `brokkr check`, whose test phase builds dev unless the sweep's
  `profile` says otherwise.
- **`doctests`** (default `false`): see doctests key below.
- **`serial_builds`** (default `false`): see serial_builds below.
- **`default_profile`**: the validation profile `brokkr check` uses when you
  pass no `--profile`. With no profile config, `brokkr check` runs every
  `[[check]]` entry without libtest filters. With no `[[check]]` either, it
  falls back to a single `--all-features` sweep.
- **`gate_profile`**: the profile `brokkr check --gate` runs. Load requires
  it to exist and to declare `certifies = "complete"`, so docs and hooks can
  say `brokkr check --gate` and stay correct through profile renames.

The legacy `[test.sweeps.*]` map is rejected at parse time. Sweeps live in
`[[check]]` entries, and profiles reference them by name. The sweep-selection
ladder `brokkr check` uses, and how `brokkr test` diverges from it, are in
`brokkr man check`.

### doctests key

The `[test] doctests` key decides whether `brokkr check`'s serial lanes run
doctests inline, outside a complete claim. It is off by default for cost:
rustdoc compiles and runs doctests in a pass of its own, apart from the test
build. Before edition 2024 it compiles each example separately. Edition 2024
merges the compatible ones, but `compile_fail`, `standalone_crate` and the
like still compile alone, which can make doctests too slow for the edit loop
in a project with many examples.

The gate checks them instead. A `certifies = "complete"` profile ignores
this key and runs every tested build's doctests in doctest carriers, unless a
`[[quarantine]] category = "doctests"` entry suppresses them (see doctests
above).

With the key `false`, each sweep's `cargo test` is scoped to `--tests`, with
no doctests, unless it already has a target selector (`--test NAME`), which
then decides the targets. Set `true` to restore the full `cargo test`
default.

With the key `false`, a run outside a complete profile skips doctests unless
a `doc_only` entry or a forwarded `--doc` selects them, and brokkr doesn't
warn. So a project that never runs a complete profile leaves its doctests
unchecked, unless it sets this key or selects them explicitly. No per-entry
or CLI switch overrides the key, and `brokkr test NAME` is unaffected. More
is in `brokkr man test-lanes`.

### serial_builds

The `[test] serial_builds` key (default `false`) runs every cargo invocation
of `brokkr check`'s clippy, rustdoc and prepare-build steps one after
another. By default they run as one chain per target directory, the chains
concurrent: the shared dir, and each isolated `unify-*` or `rustflags-*` dir.

A target dir separates everything cargo writes, but not what a build script
writes outside its `OUT_DIR`, into the source tree or a shared directory. A
project whose build scripts do that sets this key.

A cargo build dir configured anywhere (`CARGO_BUILD_BUILD_DIR`,
`build.build-dir`) holds intermediate artifacts across target dirs, and
forces one chain on its own:

- for clippy and rustdoc, when any lane they run sees one
- for the prepare builds, when a tested lane does

So a `[[lint_sweep]]` `env` reaches only the first. If brokkr can't fully
inspect the cargo config, it also uses one chain, and cargo reports
unreadable or invalid config files when the build runs.

### profiles

A `[test.profiles.NAME]` table declares a test selection layered onto one or
more `[[check]]` entries. Its fields are:

- `sweeps` (required): a list of `[[check]]` entry names
- `tests`: `--test NAME`
- `lib_tests`: `--lib`
- `exclude_tests`
- `only`: a positional substring filter
- `skip`
- `include_ignored`
- `test_threads`
- `env`
- `isolation`, `lanes`, `certifies`, `skip_phases`, `extends`,
  `description`: see below

`extends = "OTHER"` walks the chain with cycle detection. Collections are
replaced, with the child winning, and `env` merges key by key.

Profiles use Rust module paths as the annotation surface. `only` and `skip`
translate directly into cargo substring filters and `--skip`.

### filters

A `skip` entry is either a bare substring (libtest `--skip`) or a
package-qualified table, such as
`{ package = "nautilus-infrastructure", pattern = "serial_tests::" }`.
brokkr filters a qualified entry out of the enumerated set instead of
expressing it as cargo selection. That is the only way to tell identical
module paths in different packages apart, since integration-test paths have
no crate prefix. Qualified entries require `isolation = "process"`, enforced
at resolve time. A test name existing in both a skipped and an unskipped
package is a reported collision, never half-obeyed.

`skip` and `only` substrings must be at least four characters, in a profile
or a `[[check]]` entry alike; for a qualified skip, the floor is on the
`pattern` half. Shorter is a load error. A substring that short matches
nearly every test name, so it suppresses or selects tests nobody chose while
still matching something. That is the one dead-filter shape the `coverage`
phase can't detect. The fix is the substring you meant, and there is no
opt-in for a broad filter.

A filter that matches no test fails `brokkr check`, under every profile
where it can be judged. Under a `certifies = "complete"` profile, the
`coverage` phase asserts every `skip` and every `only`, profile-level and
entry-level alike, each individually, against the enumeration. It reports a
dead one with the block it was written in.

brokkr judges a filter against the sweeps it applies to. An entry's filter is
judged against the lanes running that entry, and a profile's filter against
every sweep the profile runs. So a profile-level skip naming a test outside a
package-scoped sweep is live as long as some sweep of that profile runs that
test.

The rule matches the one for a stale `[[quarantine]]` entry, for the same
reason. A filter selecting nothing is a name that drifted, and a dead `only`
leaves its lane evaluating nothing while still reading as a gate. Under any
other profile, brokkr makes the same judgement from each lane's own
unfiltered listing where that evidence is sound, and fails a green test
phase. So a `skip` left behind by a rename fails the run that contains the
rename.

The rule complements the test phase's `zero tests ran` refusal, which
catches a sweep whose filters together select no work at run time. A live
sibling `only` hides a dead one from that refusal. More is in
`brokkr man test-lanes`.

### isolation

`isolation = "process"` on a profile runs each of its tests in its own
`cargo test ... -- --exact NAME` process. `--test-threads=1` serializes
tests inside one process per test binary, but it doesn't isolate them. Tests
touching process-global state, such as a global logger, need the
fresh-process guarantee that CI's nextest gives.

brokkr reuses the sweep's selection argv verbatim per test, so the build
fingerprint is identical and cargo provides the test env. The cost is one
cargo spawn per test, sized for a serial family of a dozen tests, not
thousands. Process isolation requires `test_threads` unset or 1, never runs
doctests, and merges through `extends`. brokkr rejects `brokkr check -- ...`
extra args on an isolated sweep.

### test_threads

Two rules restrict `test_threads`.

Under a `complete` claim, `test_threads` other than 1 is a load error for any
plain libtest `[[check]]` entry the profile runs, directly or through a lane.
Cargo-mediated parallelism shares one output stream, so its executions can't
be attributed. The error names the profile and the entries. Drop
`test_threads` from that profile, or move the entries to a profile without
it, where `parallel = { budget = N }` gives them concurrency the claim can
attribute. Parallel, nextest and doc-only entries are exempt from this rule;
a count over a parallel entry falls under the next one. Either fix keeps the
entries' doctests in the claim, which runs them in a doctest carrier of each
tested build (see doctests above).

`test_threads` over a `parallel` entry, any value, is a load error for every
profile, claimed or not. That holds whether the profile sets the key or
inherits it through `extends`. The error then names the parent, and since a
child can't unset an inherited value, either the key leaves the parent or
the child stops extending it.

On a parallel entry the budget alone decides concurrency, and the profile's
count is never consulted. `1` would ask for a serial run it doesn't get, and
any other value for a tuning that does nothing. brokkr refuses the pairing
whatever the budget, including one, because an unset budget resolves from
the host, and whether a config loads must not. The error names the profile
and the entry.

For `1`, reference a non-parallel twin instead: a `[[check]]` entry with the
same build shape and no `parallel` key. For any other value, drop the key.
Where the profile also runs plain libtest entries the count does drive, keep
it on a profile of its own over those entries and compose the two with
`lanes`.

### lanes

`lanes = ["tier1", "serial"]` composes profiles as a list of runs. Each lane
resolves independently, and the test phase runs every lane's sweeps in order,
with lane-qualified labels such as `tier1/default`. The clippy phase dedupes
on build shape, so two lanes sharing a `[[check]]` entry are linted once.

Lanes are the opposite of `extends`: tier1's `skip` and serial's `only`
contradict each other by construction, so no merge can express them. These
are load errors:

- a lanes profile with keys other than `lanes`, `certifies`, `skip_phases`
  and `description`
- a lane that doesn't exist
- a nested lane
- a lane declaring `certifies` itself, since the claim belongs to the
  composing profile

Under `certifies = "complete"`, every lane must satisfy the interim
no-narrowing rule on its own. `brokkr test` under a lanes profile keeps one
sweep per build shape, since it drops filters anyway.

### certifies

`certifies = "complete"` or `"partial"` declares what a green run of the
profile claims, and permissions derive from the claim:

- **`partial`**: may set `skip_phases`, a subtractive list of check phases
  validated against the phase names, and may use `-p` scoping. It prints
  `check partial` and exits 10 on success, so
  `brokkr check && git commit` fails closed.
- **`complete`**: prints `check complete` and exits 0. It rejects
  `skip_phases`, `-p` and `extends`, since an inherited filter set defeats an
  explicit claim.

A complete profile runs the coverage phase. Every pair of build shape and
test that no lane runs must be justified by a `[[quarantine]]` entry or be
`#[ignore]`d at the source, or the check fails as orphaned. It also runs
every tested build's doctests, unless a `[[quarantine]] category =
"doctests"` entry justifies their absence (see doctests above).

A profile without `certifies` uses binary exit codes and prints
`check passed`. `extends` doesn't inherit `certifies` or `skip_phases`.

## quarantine

The `[[quarantine]]` array is the justification ledger for coverage
accounting, audited on every `certifies = "complete"` run. These entries
justify one test and the missing doctests:

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

The keys and rules are:

- An entry has exactly one of `pattern`, a test-name substring with libtest
  filter semantics, or `category`, whose only value is `"doctests"`.
  `issue` and `reason` are required: the issue ID turns the list into a
  countdown instead of a graveyard.
- `package = "PKG"` (optional, `pattern` entries only) restricts the entry to
  one package's pairs. Without it, a name-only pattern absorbs same-named
  pairs in every package. A test that later stops running for an unrelated
  reason would then count as accounted instead of orphaned. A `package` that
  isn't a workspace package justifies nothing, and `brokkr check --audit`
  names it without a build.
- A `pattern` entry justifying zero non-run pairs fails the check; delete it
  when the bug closes.
- A `doctests` entry suppresses the doctest carriers a complete run derives,
  whatever `[test] doctests` says. So no combination with that key or with
  `doc_only` entries is a load error. Every complete run under one names the
  carriers it suppressed, or says it suppressed none.
- Per-entry pair counts print on every complete run, so an entry that grows
  silently, such as a new test matching an old substring, is visible.
