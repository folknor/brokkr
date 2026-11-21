# brokkr test

`brokkr test [-p PKG] NAME` runs the cargo tests whose names match `NAME`, in
any cargo project. It shares `brokkr check`'s sweeps and profiles (`brokkr
man check`), its libtest runner and time ceilings (`brokkr man hygiene`),
and its test lanes (`brokkr man test-lanes`). pbfhogg uses this generic
command for its tests; its workload benchmarks run through `brokkr bench
NAME`. This page covers the profile, refused names, how a name is searched
and run, packages and sweeps, the output, failures and exit codes, the
flags and the per-test timeout.

## Profile

`brokkr test` builds release by default. Pass `--debug` to build the dev
profile instead: it compiles faster, and suits a failing test that isn't
profile-sensitive. `[test] debug = true` in `brokkr.toml` makes dev the
default, and `--release` forces release back. A `[[check]]` entry may pin
`profile = "dev"` or `profile = "release"`, which applies to the sweeps that
reference it.

The profile resolves in this order, first match wins:

1. `--debug` or `--release`, which are mutually exclusive
2. the sweep's `[[check]] profile`
3. `[test] debug`
4. release

The sweep's profile sits above the project-wide default on purpose. A repo
can set `[test] debug = true` for a fast inner loop, and a documented
`brokkr test SOME_RELEASE_ONLY_TIMING_TEST` still runs release, rather than
switching to dev and failing on the build profile instead of the code.

A sweep that can run only under its profile also sets `profile_required =
true`. A flag asking for the other profile then doesn't override the pin:
it leaves the sweep out of the run before anything builds, as a
package-scope exclusion does (see Packages and sweeps). The plan line names
it under `requires dev:` or `requires release:`, and a run that resolves to
that one sweep prints `SKIP PKG::NAME - requires dev; requested release`.
brokkr leaves the sweep out rather than run it at its pin, because the flag
asked to observe one profile, and a pass from the other would answer a
different question. With no flag, or the agreeing one, the sweep runs like
any pinned sweep.

## Refused names

brokkr refuses a `NAME` that matches every test in the package. As a
substring filter, `""`, `_` or a single letter turns `brokkr test` into a
whole-suite run under `--nocapture` that floods the terminal with every
test's output. The whole suite is the job of `brokkr check -p PKG`. The
refusal has two layers:

- **Blank names**: `""`, or whitespace only, is refused at parse time,
  before anything builds.
- **Match-everything names**: any other spelling is judged by what it
  matches, not how it looks. A name contained in every test that discovery
  listed for the package, benchmarks included since `cargo test` runs them,
  is refused before any test runs.

Two cases are exempt from the second layer. A package with under five tests
is exempt, since there a match-everything name is as likely a deliberate
"run this crate's tests". A doc-only sweep is exempt, since its doctests
aren't enumerated.

The rule is package-wide, so a name that leaves even one test out runs:
`::` passes in a package whose integration tests sit at the top level of
their files. A sweep that selects its targets (`tests`, `lib_tests`) is
exempt too. Its configuration made it a subset, so a name matching all of
that subset is what the configuration permits.

## Search and execution

`brokkr test` matches unit and integration tests alike by the name
substring, within the selected package, across every test harness the sweep
runs. A sweep with no target selection searches every harness of the
package: `lib`, bins, integration tests, and examples or benches with `test
= true`. It builds once with `cargo test --no-run -p PKG --tests`, the same
selection a named `cargo test -p PKG NAME` compiles, so examples aren't
built only to check that they compile.

A sweep that selects targets builds and searches exactly those, with its
`--test NAME` and `--lib` selectors in place of `--tests`. A release sweep
that exists to run one harness doesn't compile the package's other
harnesses to look for a name. Its `exclude_tests` targets are built with
the rest but left out of the search. Then:

1. **Discovery.** Every prebuilt harness lists itself with libtest's JSON
   discovery (`--list --format json`), directly and one at a time, in
   cargo's run order. brokkr uses JSON because the terse listing has no
   start or end marker: a binary that ignored `--list` and exited 0, or a
   listing cut short, reads like a real, smaller listing. A JSON listing
   opens with a `discovery` record and closes with a `completed` record
   whose counts must agree with what it listed. A listing that fails, times
   out (20s), leaves its output open after exiting, or never completes stops
   the run with the command that produced it. brokkr never reads it as a
   harness with no match. Listings run one at a time because concurrent
   listing would run every harness's static constructors at once.
2. **Matching.** brokkr applies libtest's own rule to the discovered names:
   the full name contains `NAME`, or equals it under `--exact`.
3. **Execution.** Only the harnesses that hold a match run, each as its own
   process: `BINARY NAME --include-ignored --nocapture --test-threads=1 -Z
   unstable-options --format json`. Each runs under cargo's reconstructed
   launch envelope ("Parallel test binaries" in `brokkr man test-lanes`):
   the package-root working directory, `CARGO_PKG_*`, `CARGO_MANIFEST_*`,
   build-script `rustc-env`, runtime `CARGO_BIN_EXE_*`, `[env]` config and
   cargo's loader path. A configured target runner or build target refuses the run, since
   direct execution would bypass the one and misread the other. A harness
   with no match costs one listing, not a cargo invocation.

brokkr excludes `harness = false` targets and names all of a sweep's on one
`not searched:` line. It identifies them from the owning package's
manifest, since cargo's metadata and artifact records have no harness flag.
Such a binary is arbitrary code with no libtest listing to read, and a
focused run selects libtest names.

`harness = true` isn't proof of libtest, since a `custom_test_frameworks`
runner keeps it. That is why every eligible listing must prove itself
complete, and why each executed harness must then account for exactly the
tests its listing matched. A run that reports a different number than
discovery listed (none, say) fails as `run disagrees with discovery`, never
as a `SKIP` or a pass. A listing never has to be trusted on its own word: a
harness whose listing and execution disagree can't produce a verdict.

brokkr runs one process per harness because a single `cargo test` stops at
the first harness with a failing test. The harnesses after it never run,
and a mutation check reads a short list of what went red. `--no-fail-fast`
alone doesn't fix it. A harness that dies mid-test leaves one shared
libtest stream with an open suite, and the next harness is killed as the
dead test's hang. One process per harness gives every harness its own
stream. A crash ends only its own run, and every failure belongs to the
harness that printed it, so one test name in two harnesses is two failures.

A `doc_only` sweep keeps a single cargo invocation, `cargo test -p PKG --doc
NAME`. Ordinary discovery can't see doctests, and a named `cargo test` runs
none otherwise.

brokkr always adds `--include-ignored --nocapture --test-threads=1`, plus
`-Z unstable-options --format json` to drive libtest's event stream
(`brokkr man hygiene time-ceilings-per-test-cap`). `brokkr
test` therefore needs nightly, as parallel sweeps do.

`--nocapture` is a libtest flag, and for a `doc_only` sweep it doesn't give
live output. Rustdoc captures each doctest subprocess itself, through its
own `--no-capture` option. Passing `--nocapture` through cargo's `--` split
disables only the outer libtest capture, so a doc-only sweep's doctest
output stays buffered until the doctest completes. Fixing that means
passing rustdoc its own flag through its rustdocflags. brokkr places
rustdocflags only for a sweep's `--cfg` rustflags (see `rustflags` in
`brokkr man sweeps`), not for this flag.

`cargo test NAME` is a substring filter, so tests with one name in different
modules of the same package all run. Use a more qualified name, the module
path, to tell them apart.

## Packages and sweeps

brokkr resolves the package in this order, first match wins:

1. `-p` or `--package`
2. `[test] default_package` in `brokkr.toml`
3. the project's CLI package (pbfhogg-cli, nidhogg)

A workspace such as ratatoskr must pass `-p` or set `default_package`.
Whatever the source, it must name one package. brokkr refuses a glob (`*`,
`?`, `[`), since cargo could expand it to several packages: several
packages' harnesses in one invocation, and a different feature graph.

brokkr selects sweeps in this order:

1. If `[test].default_profile` is set, the test runs against every
   `[[check]]` entry the profile references.
2. Else, if `[[check]]` is non-empty, every entry runs in declaration order.
3. Else, brokkr runs a single `--all-features` sweep.

brokkr drops the declared name filters (`only`, `skip`) and
`include_ignored`, since your `NAME` is the filter. It keeps target
selection, because that says which harnesses a sweep is, not which names
match: `tests`, `lib_tests` (entry and profile) and `exclude_tests`.

brokkr projects each sweep's selectors onto the run's one package. It first
holds a selector to the sweep's own selection: its `packages`, the
workspace less its `test_exclude_packages`, or cargo's default members. A
selector naming no target there is a config error, as cargo would make it
under `check`. Then a selector naming a target the run's package lacks
drops out, since cargo refuses `-p PKG --test NAME` for it. A sweep left
selecting nothing is out of scope (`none of their test targets here:` on
the plan line), never widened to every harness. Sweeps that select
different targets never dedupe onto each other, since they search different
tests.

Before the test phase, brokkr rebuilds each sweep's `build_packages` with
the matching feature flags, so `tests/cli_*.rs` invocations get a CLI
binary with the same feature set the test crate sees.

The resolved package always replaces each sweep's own selection. brokkr
decides scope before the build, so it skips a target that doesn't have the
sweep's features rather than force-build it into a certain `BUILD FAILED`,
its support builds included.

A sweep in scope whose harnesses hold no match, usually because of a
`#[cfg(feature = "...")]` gate, prints a `SKIP` line only when another sweep
did run the name. Only then is "feature-gated out of this sweep" real
information. When no sweep matched, there is no per-sweep line, only one
closing error: `no test matches NAME in PKG (44 harnesses searched in
default and strategy-features)`.

## Output

`brokkr test` streams the test's own stdout and stderr live. It strips
test-harness framing lines:

- standalone `ok` and `FAILED` verdict lines
- the duplicate empty `failures:` header
- the `RUST_BACKTRACE` hint
- for the doc-only cargo run, cargo's `Running` launch lines and its `to
  rerun pass ...` suggestion

Then it prints a `[test]` footer per harness that ran the name, `PASS` or
`FAIL`, tagged with the harness (`[test:cli_sort]`, `[lib:pkg]`), and
`BUILD FAILED` for a sweep whose build failed.

There are no `sweep:` header lines. Every line that belongs to one sweep
names it (`SWEEP: 44 harnesses built in 3.9s`, `SWEEP: 1 of 85 harnesses
holds a match: test:cli_sort`, the `PASS` and `FAIL` tags), and a
single-sweep run drops the prefix.

A multi-sweep run opens with one plan line rendered from the run's
selection ("Invocation selection" in `brokkr man check`). It names:

- the sweeps the run is eligible to go through
- the sweeps out of the package's scope, with their reason: `not this
  package:` for a `packages` list the target isn't in, `excluded by
  test_exclude_packages:`, `none of their test targets here:`
- the sweeps whose required profile the flag contradicts (`requires dev:`)
- the sweeps deduped onto an earlier sweep that would execute the same
  thing (`deduped: serial/all (as tier1/all)`)

That line is the only place an out-of-scope or deduped sweep is named; it
prints no `SKIP` of its own. A run that resolves to a single sweep has no
plan line, so its out-of-scope `SKIP` still prints. A deduped sweep doesn't
count toward "multi-sweep".

Like `check`, `test` opens a run log under `.brokkr/check-logs/`, named
`test-MS.log`. brokkr keeps the newest ten of these on their own, so a burst
of `test` runs never prunes the `check` log you are investigating. brokkr
reserves the log before the lock, opens it once the lock is held, and
closes it after the watchdog's join, so a run ending 124 or 130 keeps
everything up to the stop.

The cargo commands (the support builds, the test prebuild) and each
harness's invocation line (`BINARY ARGS (cwd DIR)`) go to the run log. The
console shows a build's command only when the build fails or errors out,
beside its diagnostics, the rule `brokkr check` follows. It shows a
harness's invocation only when that harness fails, after its `FAIL` footer,
as the reproduction line: once per distinct invocation under `-N`. Spawn
and runner errors also print the invocation, and a doc-only build failure
prints its cargo command. A passing single test reads as its build line,
its match line and the `PASS` footer.

brokkr builds and discovers every sweep before the first test runs. Just
before each sweep runs, brokkr builds it again and holds it to that record,
since another sweep's build may have overwritten shared outputs, and
discovery itself ran each harness's code. The second build is silent unless
it fails or finds drift. A failure prints its command beside the
diagnostics, or names the sweep for an error before cargo reports. Drift
re-prepares the sweep with full narration. Neither build prints cargo's
warnings on success.

## Failures and exit codes

The `FAIL` footer lists every failure the harness reported, one per line
when there are several, each with its panic message and location. brokkr
recovers these from the stderr stream, since `--nocapture` produces no
captured failure blocks.

A harness that dies, by a signal or a non-zero exit that nothing on the
roster explains, is a failure of its own: `test harness failed (HOW)`.
`HOW` is the harness's own exit status (`signal: 6, SIGABRT`, `exit status:
101`). For the doc-only cargo run it is cargo's `process didn't exit
successfully` detail, since cargo's exit code says nothing about how the
harness died. The harness failure sits beside whatever tests the harness had
already failed, and those survive the crash: the failed verdicts of a suite
that never summarised stand in for the name list it never printed. brokkr
offers the last test seen starting as a suspect, never as the failure
itself. It reads the suspect from the harness's own output, so a lost or
printed record can move it, and a crash before the first test names
nothing.

When a harness's only failure is one failing test, the footer leads with
that test's full name rather than your filter, which moves into the timing
parenthesis when it differs:

```text
FAIL pkg::mod::tests::full_name [default, lib:pkg] (0.25s; filter: full) - MSG @ LOC
```

Every other failure set keeps the filter-led tag. A failing harness's `[run]`
line, printed after its footer, names the working directory it runs from.

A failure counts as a test only when libtest's own verdict names it, in a
detail block or the `failures:` roster, not by a panic's thread name, which
a test can rename. Where discovery listed the harness, the name must also be
one it listed.

A hung test leads its `FAIL` line the same way when its name is a verified
identity: a run that held exactly one test, named by the resolver
(`--timeout`), killed at that test's ceiling, whether the per-test tracker
or the invocation's wall clock expired. brokkr classifies both as a
per-test timeout for that resolved name. These keep the filter-led form:

- a shared harness's sweep-wall kill, which has no verified offender
- an idle kill, which has no test to name
- a shared harness's per-test kill, which names only a suspect

The exit code is non-zero if any run was `FAIL` or `BUILD FAILED`, or if
every sweep found no match (a bad name). A no-match sweep beside at least
one `PASS` exits `0`. The closing error line names what failed, the detail
having already printed:

- `test failed` when a test failed
- `build failed` when only builds did
- `test failed and build failed` when both did
- for a bad name, the `no test matches` sentence itself, printed once

Under `-N`, a no-match sweep's `SKIP` line, when it prints, comes from run 1
only. A fired `test` phase ceiling exits 124, and a graceful `brokkr kill`
or Ctrl-C exits 130 (`brokkr man hygiene`).

## Flags

`brokkr test` takes these flags:

- **`-N COUNT`**: repeat the test, per sweep, to hunt a flaky test. The
  sweep builds and discovers once; each iteration re-runs every matching
  harness. The build and match lines print for run 1 only. The first
  occurrence of each distinct failure set prints its full block, and repeats
  of the same set collapse to their `[test] FAIL` footer. A closing `[test]
  summary:` line gives `PASS` and `FAIL` counts plus one `Nx` group per
  distinct failure set, with its further members on `+` lines. A failure set
  is every failure of an iteration across all harnesses, each keyed by
  harness, test and panic location, or the message when there is no
  location. brokkr keys on the whole set rather than the first failure, so
  an iteration that failed A and B never folds into one that failed A
  alone.
- **`-j COUNT`**: cargo `-j COUNT` for parallel compile.
- **`--debug`**: the dev profile instead of release. It overrides `[test]
  debug` and a sweep's `[[check]] profile`; a sweep with `profile =
  "release"` and `profile_required = true` is left out instead.
- **`--release`**: force release, overriding `[test] debug = true` and a
  sweep's `profile = "dev"`. A sweep that requires dev
  (`profile_required`) is left out instead. It is mutually exclusive with
  `--debug`.
- **`--timeout SECS`**: raise the per-test watchdog ceiling, 1 to 280s
  (see Per-test timeout).
- **`--timings`**: after the run, print every test that ran, slowest first,
  with its wall time, as `brokkr check --timings` does. The time is the
  per-test tracker's clock, from libtest's start event to its result, with
  the build excluded. It prints however the run ended, failed or stopped
  included. A test killed at its cap is listed with the time it had run,
  marked `killed at the ... cap, did not finish`. Under several sweeps, or
  `-N`, each line names its sweep, and run. With `--timeout`, this finds
  every test over the standard cap: `brokkr test -p foo --timeout 280
  --timings some_module::`.

## Per-test timeout

A per-test watchdog, shared with `brokkr check`'s test phase, names any test
that runs longer than 20s and reports it as a hung test. `--timeout SECS`
raises that ceiling for `brokkr test` only, and always for one isolated
test. Every (harness, test) pair that `NAME` matches in a sweep's discovery
runs as its own process, one after another, so one name in two harnesses
counts twice. A sweep where the name matches no test (feature-gated out) is
fine, and a `SKIP` line notes it when another sweep ran the name.

`--timeout` is the only exception to the 20s cap anywhere in brokkr, and
280s is its hard limit: you can't disable the ceiling. Exceeding the cap,
raised or not, fails the run and stops it, including between `-N`
iterations.

Because it promises a per-test ceiling, `--timeout` also makes attribution
exact. Discovery resolves `NAME` to each matching full test name in its
harness, and brokkr invokes the binary once per name with libtest
`--exact`. Each process is the unit of one test, so brokkr names a kill by
its own selection rather than by anything parsed. Resolving the full name
is load-bearing: `--exact` applied to the substring you typed would match
nothing, and the run would silently execute no tests.

`--timeout SECS` bounds a process, not a test body. It runs from the
harness process's spawn to its exit, covering static constructors, the test
itself and teardown. A constructor can spend the budget without the test
body hanging, and the report then says the invocation selected for the test
exceeded it. The build and every other harness's listing fall outside it,
and so does the short grace brokkr gives the output pipes to close after
exit.
