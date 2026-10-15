# brokkr test

`brokkr test [-p <PKG>] <NAME>`. (Any cargo project.) It shares `brokkr
check`'s sweeps and profiles (`brokkr man check`), its libtest runner and time
ceilings (`brokkr man hygiene`), and its test lanes (`brokkr man test-lanes`).

Run the cargo tests matching `<NAME>`. Defaults to release; pass `--debug` to run the
dev profile instead (faster compile, useful when the failing test isn't
profile-sensitive). Setting `[test] debug = true` in `brokkr.toml` flips the
default to dev; `--release` forces release back. A `[[check]]` entry may pin
its own `profile = "dev" | "release"`, which applies to the sweeps that
reference it. Precedence: `--debug` / `--release` (mutually exclusive) >
the sweep's `[[check]] profile` > `[test] debug` > release.

The sweep's profile sits above the project-wide default deliberately: a repo
can take `[test] debug = true` for the fast inner loop without the documented
`brokkr test <a release-only timing test>` quietly switching to dev and failing
on the build profile rather than on the code.

A `<NAME>` that matches every test in the package is refused. As a substring
filter, `""`, `_` or a single letter turns `brokkr test` into a whole-suite
run under `--nocapture` that floods the terminal with every test's output;
the whole suite is `brokkr check -p <pkg>`'s job. Two layers:

- A blank name (`""`, or whitespace only) is refused at parse time, before
  anything builds.
- Every other spelling is judged by what it matches, not how it looks: a name
  every test discovery listed for the package contains (benchmarks included,
  since `cargo test` runs them) is refused before any test runs. Packages under
  five tests are exempt - there a match-everything name is as likely a
  deliberate "run this crate's tests" - and so are doc-only sweeps (not
  enumerated).

It is a package-wide rule, so a name that leaves even one test out runs:
`::` passes in a package whose integration tests sit at the top level of
their files.

Unit tests and integration tests are both matched by the name substring within
the selected package, across every test harness of the package (`lib`, bins,
integration tests, and examples or benches with `test = true`). Each sweep
builds once - `cargo test --no-run -p <pkg> --tests`, the same selection a
named `cargo test -p <pkg> <name>` compiles, so examples are not built merely
to check they compile. Then:

1. **Discovery.** Every prebuilt harness lists itself, directly and one at a
   time in cargo's run order, with libtest's JSON discovery (`--list --format
   json`). JSON rather than the terse listing because terse output has no start
   or end marker: a binary that ignored `--list` and exited 0, or a listing cut
   short after a few lines, reads exactly like a real, smaller listing. A JSON
   listing opens with a `discovery` record and closes with a `completed` record
   whose counts must agree with what it listed. A listing that fails, times out
   (20s), leaves its output open after exiting, or never completes stops the
   run with the command that produced it - it is never read as a harness with
   no match. One at a time, because concurrent listing would run every
   harness's static constructors at once, which no earlier run did.
2. **Matching.** libtest's own rule, applied to the discovered names: the full
   name contains `<NAME>` (equals it under `--exact`).
3. **Execution.** Only the harnesses holding a match run, each directly as its
   own process (`<binary> <NAME> --include-ignored --nocapture
   --test-threads=1 -Z unstable-options --format json`) under cargo's
   reconstructed launch envelope - package-root cwd, `CARGO_PKG_*`,
   `CARGO_MANIFEST_*`, build-script `rustc-env`, runtime `CARGO_BIN_EXE_*`,
   `[env]` config and cargo's loader path (see "Parallel test binaries" in `brokkr man test-lanes`). A configured target runner or build target refuses the run,
   since direct execution would bypass the one and misread the other.
   A harness with no match costs one listing, where it used to cost a cargo
   invocation - on a package of 85 harnesses that was 17 minutes of lock around
   a test that passed in 0.16s.

`harness = false` targets are excluded and all of a sweep's are named on one
`not searched:` line. They are identified from the owning package's manifest, since cargo's
metadata and artifact records carry no harness flag; such a binary is arbitrary
code with no libtest listing to read, and a focused run selects libtest names.

`harness = true` is not proof of libtest, though - a `custom_test_frameworks`
runner keeps it - which is why every eligible listing must prove itself
complete, and why each executed harness must then **account for exactly the
tests its listing matched**: a run reporting a different number than discovery
listed (none, say) fails as `run disagrees with discovery`, never as a `SKIP`
or a pass. A listing therefore never has to be trusted on its own word; a
harness whose listing and execution disagree cannot produce a verdict.

Why one process per harness: a single `cargo test` stops at the first harness
with a failing test, so the harnesses after it never run and a mutation check
reads a short list of what went red. `--no-fail-fast` alone is not the fix, for
the reason described under the sweep's `--no-fail-fast` above: a harness that
dies mid-test leaves one shared libtest stream with an open suite, and the next
harness gets killed as the dead test's hang. One process per harness makes every
stream its own. A crash ends only its own run, and every failure belongs to the
harness that printed it, so the same test name in two harnesses is two failures.

Package resolution: explicit `-p/--package` > `[test] default_package` in
`brokkr.toml` > `Project::cli_package()` (pbfhogg-cli, nidhogg); workspaces
(e.g. ratatoskr) must pass `-p` or set `default_package`. Whatever the source,
it must name one package: a glob (`*`, `?`, `[`) is refused, since cargo could
expand it to several packages - several packages' harnesses in one invocation,
and a different feature graph.

A `doc_only` sweep keeps a single cargo invocation, `cargo test -p <pkg> --doc
<name>`: ordinary discovery cannot see doctests, and a named `cargo test` runs
none otherwise.

Always adds `--include-ignored --nocapture --test-threads=1`, plus
`-Z unstable-options --format json` to drive libtest's event stream (see "The
per-test budget is a hard cap" in `brokkr man hygiene`). Nightly is therefore a prerequisite for
`brokkr test`, as it already was for parallel sweeps.

`--nocapture` is a **libtest** flag, and for a `doc_only` sweep it does not
deliver live output. Rustdoc captures each doctest subprocess itself, through
its own separate `--no-capture` option; passing `--nocapture` via cargo's `--`
split disables only the outer libtest capture. So a doc-only sweep's doctest
output is still buffered until the doctest completes. Fixing that means passing
rustdoc its own flag through `RUSTDOCFLAGS`, which brokkr does not do yet.

Sweep selection: if `[test].default_profile` is set, the test runs against
every `[[check]]` entry the profile references (profile filters are dropped -
the user's `<NAME>` is the filter); else if `[[check]]` is non-empty, every
entry runs in declaration order; else fall back to a single `--all-features`
sweep. Each sweep's `build_packages` are rebuilt with the matching feature
flags before the test phase, so `tests/cli_*.rs` invocations get a CLI binary
with the same feature set the test crate sees.

Streams the test's own stdout/stderr live (test-harness framing lines are
stripped, including standalone `ok`/`FAILED` verdict lines, the duplicate empty
`failures:` header, the `RUST_BACKTRACE` hint, and for the doc-only cargo run
cargo's `Running` launch lines and `to rerun pass ...` suggestion), then prints
a `[test]` footer per harness that ran the name - `PASS` or `FAIL`, tagged with
the harness (`[test:cli_sort]`, `[lib:pkg]`) - and `BUILD FAILED` for a sweep
whose build failed. There are no `sweep:` header lines: every line that belongs
to one sweep names it (`<sweep>: 44 harnesses built in 3.9s`,
`<sweep>: 1 of 85 harnesses holds a match: test:cli_sort`, the PASS/FAIL tags),
and a single-sweep run drops the prefix. A multi-sweep run opens with one plan
line rendered from the run's selection (see "Invocation selection" in `brokkr man check`), naming the
sweeps it is eligible to go through, those out of the package's scope with
their reason (`not this package:` for a `packages` list the target isn't in,
`excluded by test_exclude_packages:`), and those deduped onto an earlier sweep
that would execute the same thing (`deduped: serial/all (as tier1/all)`). That
line is the only place an out-of-scope or deduped sweep is named: it prints no
`SKIP` of its own (a run resolving to a single sweep has no plan line, so its
out-of-scope `SKIP` still prints). A deduped sweep does not count toward
"multi-sweep". The resolved package always replaces each sweep's own
selection, and the scope decision is made *before* the build, so a target
that doesn't carry the sweep's features is skipped rather than force-built
into a guaranteed `BUILD FAILED` - its support builds included.

A sweep in scope whose harnesses hold no match (usually
`#[cfg(feature = "...")]`-gated) prints a `SKIP` line only when another sweep
did run the name - then "feature-gated out of this sweep" is real information.
When no sweep matched, there is no per-sweep line: one closing error says
`no test matches <NAME> in <pkg> (44 harnesses searched in default and
strategy-features)`.

Like `check`, `test` opens a run log under `.brokkr/check-logs/`, named
`test-<ms>.log` and retained ten deep on its own, so a burst of `test` runs
never prunes the `check` log being investigated. It is reserved before the
lock, opens once the lock is held, and closes after the watchdog's join, so a run ending 124 or 130 keeps everything
up to the stop.
The cargo commands (the support builds, the test prebuild) and each harness's
invocation line (`<binary> <args> (cwd <dir>)`) go to the run log. The console
shows a build's command only when the build fails or errors out, beside its
diagnostics - the rule `brokkr check` follows - and a harness's invocation only
when that harness fails, after its `FAIL` footer, as the reproduction line (once
per distinct invocation for the command under `-N`). Spawn and runner errors
also print the invocation; a doc-only build failure prints its cargo command.
A passing single test therefore reads as its build line,
its match line and the `PASS` footer.

Every sweep is built and discovered before the first test runs, then built
again just before it runs and held to that record (another sweep's build may
have overwritten shared outputs, and discovery itself ran each harness's code).
That second build is silent unless it fails - then its command (or, for an
error before cargo reports, the sweep) prints beside the diagnostics - or finds
drift, which re-prepares the sweep with full narration. Neither build prints
cargo's warnings on success.

The `FAIL` footer lists **every** failure the harness reported, one per line
when there are several, each with its panic message and location (recovered
from the stderr stream, since `--nocapture` produces no captured failure
blocks). A harness that dies (a signal, a non-zero exit with nothing on the
roster to explain it) is a failure of its own, `test harness failed (<how>)`,
where `<how>` is the harness's own exit status (`signal: 6, SIGABRT`, `exit
status: 101`) - or, for the doc-only cargo run, cargo's `process didn't exit
successfully` detail, since cargo's exit code says nothing about how the
harness died. It sits beside
whatever tests that harness had already failed. Those survive the crash: the
failed verdicts of a suite that never summarised stand in for the name list
it never printed. The last test seen starting is offered as a **suspect**,
never as the failure itself: it is read from the harness's own output, so a
lost or printed record can move it, and a crash before the first test names
nothing.

When a harness's only failure is one failing test, the footer leads with that
test's full name rather than the user's filter, which moves into the timing
parenthesis when it differs (`FAIL pkg::mod::tests::full_name [default,
lib:pkg] (0.25s; filter: full) - <msg> @ <loc>`); every other failure set keeps
the filter-led tag. A failing harness's `[run]` line (printed after its footer)
names the cwd it runs from.
A failure counts as a test only when libtest's own verdict names it (a detail
block or the `failures:` roster, not a panic's thread name, which a test can
rename) and, where discovery listed the harness, the name is one it listed. A hung test leads its
`FAIL` line the same way when its name is a verified identity: a run that held
exactly one test, named by the resolver (`--timeout`), and was killed at that
test's ceiling, whether the per-test tracker or the invocation's wall clock
expired. Both are classified as a per-test timeout for that resolved name.
A shared harness's sweep-wall kill has no verified offender, an idle kill has
no test to name, and a shared harness's per-test kill names only a suspect,
so those keep the filter-led form. Exit code:
non-zero if any run was `FAIL`/`BUILD FAILED`, or if *every* sweep found no
match (bad name); a no-match sweep mixed with at least one `PASS` exits `0`. The
closing error line names what failed, the detail having already printed: `test
failed` when a test failed, `build failed` when only builds did, `test failed
and build failed` when both, and for a bad name the `no test matches` sentence
itself, printed once. Under `-N`, a no-match sweep's `SKIP` line (when it
prints) comes from run 1 only. A fired `test` phase ceiling exits 124 and a
graceful `brokkr kill` / Ctrl-C exits 130 (see `brokkr man hygiene`).

Flags:
- `-N <n>` - repeat the test (per sweep) for flaky-test hunting. The sweep
  builds and discovers once; each iteration re-runs every matching harness.
  The build and match lines print for run 1 only. The first occurrence
  of each distinct failure set prints its full block; repeats of the same set
  collapse to their `[test] FAIL` footer alone. A closing `[test] summary:`
  line gives PASS/FAIL counts plus one `Nx` group per distinct failure SET -
  every failure of an iteration across all harnesses, each keyed by harness,
  test and panic location (the message when there is no location), with its
  further members on `+` lines. Keyed on the whole set rather than the first
  failure, so an iteration that failed A and B never folds into one that
  failed A alone
- `-j <n>` - cargo `-j N` for parallel compile
- `--debug` - dev profile instead of release (overrides both `[test] debug` and
  a sweep's `[[check]] profile`)
- `--release` - force release, overriding `[test] debug = true` and a sweep's
  `profile = "dev"` (mutually exclusive with `--debug`)
- `--timeout <SECS>` - raise the per-test watchdog ceiling (1-280s)

Because `cargo test <name>` is a substring filter, identically-named tests in
different modules of the same package all run; use a more qualified name
(module path) to disambiguate.

A per-test watchdog (shared with `brokkr check`'s test phase) names any test that
runs longer than 20s and reports it as a hung test. `--timeout <SECS>` raises
that ceiling for `brokkr test` only, and only for a genuinely single test: if
`<NAME>` matches more than one (harness, test) in any sweep's discovery - the
same name in two harnesses counts twice - the command errors before running
anything, listing them. Sweeps where the name matches zero tests (feature-gated
out) are fine and are noted by a `SKIP` line when another sweep ran the name. There is no way to disable the ceiling entirely
- 280s is the cap.

`--timeout` is the **only** exception to the 20s cap anywhere in brokkr, and
280s is its hard limit. Exceeding the cap - raised or not - fails the run and
stops it, including between `-N` iterations.

Because it promises a per-test ceiling and already refuses more than one match,
`--timeout` also makes the *attribution* exact: discovery resolves `<NAME>` to
the one **full test name** in its one harness, and that binary is invoked with
libtest `--exact`, so the process is the unit of one test and the kill is named
by the caller's selection rather than by anything parsed. Resolving the full
name is load-bearing - `--exact` applied to the substring you typed would match
nothing, so the run would silently execute zero tests.

One honest caveat about what `--timeout <SECS>` bounds: it is a **process
ceiling**, not a test-body ceiling. It runs from the harness process's spawn to
its exit - static constructors, the test itself, teardown - so a constructor
can spend the budget without the test body hanging, and the report says the
invocation selected for the test exceeded it. The build and every other
harness's listing fall outside it, and so does the short grace brokkr gives
the output pipes to close after exit.
