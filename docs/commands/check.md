# brokkr check

This page covers `brokkr check` and the parts of `brokkr test` it shares: the
command, its flags and output, sweep selection, and every phase up to the
tests. The reference part starts at "Usage" and runs through one section per
phase, then "Exit codes", "Sweep log lines" and "Sweep selection".

`check` is the full validation pass. It runs these phases:

- gremlins, header, textlint, manifest and bench
- script checks, dependency rules and publish cycle
- clippy and the optional rustdoc
- prepare and the tests
- coverage, under a complete claim
- member-check and fixture-lint, when configured

`brokkr test` runs the cargo tests matching one name against the same sweep
set. The config behind both (`[[check]]`, `[[dependency_rule]]`, `[test]`,
profiles) is in `brokkr man config` and `brokkr man sweeps`.
The rest lives in sibling topics:

- `brokkr man test-lanes`: the test phase and its lanes (serial, parallel,
  process-isolated, nextest, doctests), coverage, diagnostic continuation,
  feature unification, member-check and dead filters
- `brokkr man conventions fixture_lint`: the fixture-lint phase
- `brokkr man test`: `brokkr test`
- `brokkr man hygiene`: time ceilings and the per-test cap, strays, the rustc
  guard

## Usage

You pass test arguments to `brokkr check` after a leading `--`. brokkr splits
them on a second literal `--`:

- Tokens before it go to `cargo test`: `brokkr check -- --test cli_sort`
  scopes the run to one test crate.
- Tokens after it go to libtest, after the default `--test-threads=1`:
  `brokkr check -- -- --ignored`.

With no second separator, every token is cargo-level. The leading `--` is
required: `check` doesn't collect trailing arguments, so an unrecognised
brokkr flag is a parse error rather than a token silently forwarded to
`cargo test`.

The test phase fails when a successful `cargo test` ran zero tests (no
suites, or filters excluded everything), so a too-narrow profile and filter
combination can't green-light a check. A unit that legitimately ran zero
tests, with nothing filtered out, still passes. An all-doctest crate is one,
since `--tests` excludes doctests. The test line names such a unit as
`no tests ran in SWEEP` rather than folding it into the total. On an explicit
`-p PKG` spot-check that ran nothing, an extra warning says the green
validated clippy, not tests. Only the hand-typed `-p` path gets that warning,
so a whole-workspace run never nags.

## Lock wait

`check` and `test`, like every locked brokkr command, wait for the global
per-user lock instead of failing. If another brokkr command holds it (a bench
run, say), the command prints `[lock] waiting for the brokkr lock ...` and
proceeds once the lock is released. It never fails with
`lock: already locked`, so you have no lock error to handle.

The wait is serialization, not machine congestion: the lock exists so brokkr
commands never overlap. The line `[lock] acquired - you now hold the global
brokkr lock for the duration of this command` marks the point after which
this command has the lock to itself. The wait has no effect on any timing or
result measured after it. We keep both lines' wording fixed on purpose: every
terser revision was misread by agents as other brokkr commands contending for
the machine.

`brokkr lock` shows who holds the lock and what it's doing. brokkr asks the
holder itself over its control socket, so this works across sandboxes. When
the holder recorded the same `CODEX_THREAD_ID` as the waiting command, the
wait says so: the holder is almost certainly the agent's own earlier command,
still running in another exec session.

A command doesn't wait for its own ancestor. A brokkr started beneath a locked
brokkr refuses at once instead (see `brokkr man hygiene descendant`).

## Flags

`check` takes these flags:

- **`-p`, `--package PKG`** (repeatable): scope every sweep's cargo
  invocation (clippy, rustdoc, test, member-check) to the named packages. See
  "Package scope" below. A `certifies = "complete"` profile rejects it.
- **`--features`, `--no-default-features`**: run one ad-hoc sweep with no
  `build_packages`. These override sweep selection only; the resolved
  profile's run shaping (skips, filters, thread policy) still applies. See
  "Sweep selection".
- **`--profile NAME`**: select a `[test.profiles]` entry. Conflicts with
  `--features` and `--no-default-features`.
- **`--gate`**: run the profile `[test] gate_profile` names, which brokkr
  checks at load to certify `complete`. This is the stable pre-commit
  invocation. Conflicts with `--profile`, `--features`,
  `--no-default-features` and `-p`. Any `complete` claim rejects trailing
  `-- ...` test args (see "Exit codes").
- **`--json`**: append one machine-readable summary object as the last line
  of stdout. Human output, warnings included, stays on stdout. See
  "JSON summary".
- **`--fix-gremlins`**: rewrite banned characters in place before the scan.
- **`--commands`**: print every sweep's full cargo command instead of the
  collapsed shape line. A failing sweep prints its command either way (see
  "Sweep log lines").
- **`--timings`**: after the check, print every test that ran, slowest
  first, by wall-clock time with build time excluded.
- **`--audit`**: explain the run without running it (see "Audit").
- **`--textlint NAME`, `--script NAME`**: run only the named entries (see
  "Selected entries").
- **`--force-rust`**: waive the markdown-only shortcut (see "Markdown-only
  shortcut").

## Package scope

The `-p` set replaces each sweep's own package selection. Cargo unions
selection flags, so composing `--workspace --exclude ...` with `--package`
would silently un-scope the run.

Per sweep, brokkr intersects the set with the sweep's scope. A package that
the sweep's `packages` list, or (in the test phase only) its
`test_exclude_packages`, rules out is dropped. A sweep that keeps none is
excluded from that phase, as `brokkr test` reports SKIP. So `-p a -p b` still
reaches `a` in the sweep that admits it when `b` lives in another sweep. An
excluded sweep builds nothing for that phase, its `build_packages` support
binaries included. If every applicable sweep is excluded, the phase fails
rather than reading as green (see "Invocation selection" for when).

The `invocation:` line up front shows `-p PKG ...`. When the sweeps' package
rules narrow the selection, one `package rules narrow this selection`
announcement follows the phase announcements. It groups the sweeps by what
each phase made of them:

```text
  vm, runner: excluded (not admitted - -p x is not in this sweep's packages list)
```

When the phases differ, it names them apart (`clippy: eligible | test:
eligible with -p b (not admitted - ...)`). Per phase it reports the
admission (the dropped packages) and the disposition: eligible, excluded,
deduped (and onto which sweep), or not applicable. A phase the run skips is
left out. Eligible means planned, never "will run": an earlier failure or a
stop can still end the run first.

The per-phase `skipped`, `dropped` and `deduped` lines go to the run log only.
`brokkr clippy`, which has no announcement, still prints them. The `--json`
summary has a `package` field, comma-joined for a multi-package run.

## Output

A passing run prints one line per phase, not per sweep. An LLM runs `check`
many times a session and pays for every green line on every run, so a green
run prints only what can change what you do next. A five-sweep run reads:

```text
[run]     gremlins, textlint (94 rules, 1043 files), manifest, script-check (14 checks), dependency rules (21 rules, 17 workspace packages), publish cycle: ok in 0.6s
[run]     clippy: ok (5 sweeps) in 1m12s
[run]     rustdoc: ok (4 sweeps) in 58.0s
[run]     script-check: ok (2 checks)
[warn]    cargo test: 0 errors, 1 warning
[warn]      warning broadarrow-ba v0.1.0 (...) ignoring invalid dependency `broadarrow-ba` which is missing a lib target
[warn]      (sweeps: threaded/telemetry-features, doc-twin/doctests)
[run]     test: 6546 passed (5 sweeps; slowest parallel binary broadarrow-daemon/broadarrow_daemon 32.8s in threaded/default) in 4m35s
[result]  check passed in 7m01s (profile standard, 5 sweeps; lints allowed: rustdoc::private_intra_doc_links)
```

The convention phases, gremlins through publish cycle, share one line with
their counts. Each build phase prints one line with its wall time as it
completes:

- clippy and rustdoc
- prepare, on a complete profile (any other run logs it)
- test, which closes with `test: N passed (S sweeps; ...)`, adding
  `, M ignored` and `, K filtered out` when non-zero, so a green run always
  says how much it ran
- coverage, which adds the accounting line
- member-check and fixture-lint

brokkr streams these per phase rather than rendering them at the end, so a run
the watchdog kills still shows which phases finished. A pre-test or post-test
script-check stage keeps its own line.

A green run moves the rest to the run log (see "Run log"); nothing is
dropped. That covers:

- each sweep's shape and cargo argv
- per-sweep counts and build and fan-out times
- the parallel plan and the process-isolated roll-call
- config-derived skips and build-shape dedupes
- the feature-token drops of a narrowed run

### What prints on green

Some lines print on a green run because each changes what the verdict means:

- An `invocation:` line up front naming what this run changed about the
  configured gate: `-p`, a bounded preview of forwarded `-- ...` args, and
  `RUSTFLAGS` or `CARGO_ENCODED_RUSTFLAGS` inherited from the environment.
  A set-but-empty variable counts, since cargo reads an empty one as a live
  source that shadows the config-file rustflags.
- The ad-hoc features, `skip_phases` and markdown-only announcements.
- `-p` drops and skips per sweep.
- Warnings (see "Warnings").
- Zero-test units on the test line.
- The verdict line's context: the profile, the sweeps that ran, and a bounded
  summary of `[lints]` suppressions (six names per group, with an omitted
  count for longer lists).

The suppressions sit on the verdict rather than a phase line because they
narrow several phases alike: clippy (`-A`), rustdoc (dropped at ingestion)
and every compiling phase (test, coverage, member-check, through rustflags).
Fixture-lint is the exception; it takes no allows. `allow_exact` lists its
lint names only; its scope is fixed and stated under "clippy allow_exact".

`cargo::` lints are listed in a separate `(sited)` group, because brokkr never
passes them to a build. A script that builds its own `-A` list from `[lints]`
has to skip them too, or rustc fails every crate with E0602. The verdict
prints on a red run too, with its context.

### Warnings

brokkr prints each warning once. It parses compiler and manifest warnings from
a passing test sweep or support pre-build, filters them by owner,
deduplicates them by typed fields and sorts them. It holds identical rendered
blocks until the phase ends and prints each once with the sweeps that
produced it (`every sweep` when it's all of them).

Two more warnings fire on otherwise green runs:

- A parallel plan in which two or more binaries each claimed the whole
  in-flight budget (budget above 1) warns, since those binaries couldn't
  overlap.
- A `[lints]` injection whose sink brokkr chose on a guess (a
  `target.*.rustflags` selector it can't evaluate) warns that the allows may
  be inert.

### Failures

A failure prints what the green run left out. Each failing sweep prints its
shape (`test threaded/default: default selection, unification workspace,
parallel`) and its `failing command:` beside its diagnostics. The shape's
package part says what the run selected:

- `-p a -p b` for an override or a one-package selection
- `N pkgs` for a sweep's own list
- `workspace -N pkgs` for `test_exclude_packages`
- `default selection` when no package flag is passed: cargo's default
  members, which is not a claim about the whole workspace

Every failure site has its own context. The diagnostic phases report after
all their sweeps ran, and a parallel lane after its join, so no ambient
"current sweep" could say which one a failure belongs to.

A failure whose site printed its own findings (gremlins, lint diagnostics,
the test failure list, a coverage worksheet) ends in `check failed in ...`
alone. Any other error prints its message on the line above the verdict, so a
red run always names its cause. Such errors include an IO error, a
`cargo metadata` failure, a config refusal found at phase time and a spawn
failure.

### Interrupts

A graceful `brokkr kill` or Ctrl-C ends a run as `check interrupted in ...`
and exits 130, not 1, like every other locked command. No clean follows, so a
hung-test snapshot the run captured stays. For the whole run, `check` kills
the registered test groups as soon as the signal arrives, every runner that
polls the stop flag returns, and no new test group is spawned.

A second signal while brokkr is still honouring the first takes the default
action, so a wait that doesn't poll can't swallow the interrupt. `--json`
still emits its trailer, with verdict `"failed"`. `brokkr test` and
`brokkr clippy` behave the same way.

### Run log

Each run tries to write `.brokkr/check-logs/check-MS.log` under the config
dir, keeping the newest ten `check-` logs. `brokkr test`, measured runs and
`brokkr crit` write `test-`, `measure-` and `crit-` logs beside them, and
brokkr prunes each kind against its own ten only.

brokkr reserves the log before taking the lock and opens it under the lock,
before the compile-lease drain and toolchain narration. Waiting commands
can't open or prune logs. An open failure warns and keeps history. Names
advance past the newest same-kind stamp after a clock rollback, so retention
still prunes in creation order. If listing the directory fails, brokkr warns
and skips retention for that run. A log it couldn't remove warns too, since a
silent failure would defeat the bound.

The log keeps every line printed after it opened, plus the narration a green
run doesn't print:

- shapes and full cargo argv
- the `phase NAME: started` line of every phase, followed by
  `phase NAME: not configured` when the phase had nothing to run, so an inert
  phase never reads as one that checked something
- the `[lints]` sink
- per-unit counts, and build and fan-out times
- the parallel plan and each sweep's slowest binary
- the roll-call

brokkr writes and flushes each event as it happens, so a watchdog-killed or
panicked run keeps everything up to the point it stopped. The watchdog's own
kill lines go in too, and ten logs are kept so that run survives the retries
after it. A cross-host comparison or a slow-build investigation reads from
here.

### Status line

On a terminal, one transient status line under the output names what is
running: the phase, the sweep, the process-isolated test in flight. brokkr
clears it from under every persistent line and at exit, and never draws it
when stdout is a pipe or a file, so captured output doesn't change.

### Diagnostic lines

Each diagnostic becomes one line, with compilation noise stripped and passing
tests aggregated. There is no unfiltered mode: these renderers don't use
rustc's rendered form (source excerpt, carets, `help:` spans). brokkr reports
a Cargo record it can't decode with its original JSON line, so the compiler's
message is visible even for a deeply nested macro backtrace.

A failed nextest pre-build prints the rendered compiler errors instead, with
their notes and help, and omits that build's warning-level messages. The
one-line form keeps a thirty-error run readable; a gate that turns into a
scroll gets read as one.

### Diagnostic order

Every diagnostic phase (gremlins, header, textlint, manifest, dependency
rules, clippy, rustdoc) chooses what to show by three rules:

1. **Everything is an error**, whatever level the tool reported, except a
   warning from a dependency: a package outside the workspace, told apart by
   the `package_id` cargo attaches to each message. A dependency's warning
   neither fails the phase nor prints; a dependency's error still fails.
2. **Errors in files with unstaged changes come first**, then every other
   error. Unstaged means the working tree against the index (`git diff`) plus
   untracked, non-ignored files; staged and committed changes don't count.
   The error in the file you're editing is the one to fix first. Clippy and
   rustdoc diagnostics are sorted by lint code, file and line within each
   group, so every hit of a rule clumps together.
3. **Nothing is capped.** Every error prints every time, and no flag changes
   that. A cap plus an escape hatch would make the default view of a bad run
   a lie you have to know to re-ask about.

## JSON summary

`--json` appends one summary object as the last line of stdout, leaving the
human output, warnings included, untouched. Read the final line for the
object. This is the result contract. Its fields:

- **`schema`**: currently 2.
- **`certifies`**: the resolved profile's claim, `null` for unclaimed
  profiles.
- **`verdict`**: `"passed"`, `"complete"`, `"partial"` or `"failed"`.
- **`profile`**: the profile that drove sweep selection, `null` for ad-hoc
  and legacy runs.
- **`sweeps`**: the labels of the sweeps clippy, rustdoc or the test phase
  started work on (see "Invocation selection").
- **`package`**: the CLI `-p` scope, comma-joined for several packages,
  `null` when the run wasn't scoped.
- **`failed_phase`**: `null` on success, else one of `gremlins`, `header`,
  `textlint`, `manifest`, `script_check`, `dependency_rules`,
  `publish_cycle`, `clippy`, `rustdoc`, `prepare`, `test`, `coverage`,
  `member_check` or `fixture_lint`.
- **`scope`**: `"prose_only"` when the markdown-only shortcut skipped the
  build phases, `null` on a full run. Without it a shortened `passed` reads as
  a full one.
- **`termination`**: how the run stopped, when something stopped it, as
  `{kind, scope}` (see `brokkr man test-lanes coverage-summary-objects`).
- **`policy_coverage`, `execution_accounting`, `doctests`**: set on complete
  profiles whose `prepare` phase ran, `null` otherwise (see
  `brokkr man test-lanes coverage`).
- **`diagnostic_continuation`**: set on a failed run, any profile, that left
  executions unresolved (see `brokkr man test-lanes diagnostic-continuation`).
- **`elapsed_ms`**: the run's wall time.

The object is versioned and additive within a version. Fields are only ever
added, consumers must tolerate unknown fields, and a bump is reserved for
renames or semantic changes. Schema 2 was such a change: it replaced schema
1's `coverage` object, whose `run` count meant "a lane the test phase reached
listed it", with the separate policy and execution objects.

A config error before the phases run (a bad profile name, conflicting flags, a
certifies violation) emits no summary: resolve-time errors aren't run
verdicts.

## Invocation selection

brokkr decides once, before any phase runs, which sweeps each phase attempts
and with which packages, and every phase reads that one answer. Per phase a
sweep is one of:

- **eligible**
- **excluded**: the `-p` set names none of its packages
- **deduped** onto an earlier sweep that compiles the same thing (clippy and
  rustdoc only; `check`'s test lanes never dedupe)
- **not applicable**: rustdoc on a `doc_only` carrier, and clippy and rustdoc
  on a doctest carrier a complete claim derived
- **disabled**: the phase doesn't run

A sweep that isn't eligible builds nothing for that phase, its
`build_packages` included.

A `certifies = "complete"` run first adds the doctest carriers its tested
builds are owed: one `ENTRY (doctests)` lane per build that no `doc_only`
entry carries, named at the top of the run (see Doctests under `parallel` in
`brokkr man sweeps`).

The `[[lint_sweep]]` entries the run's claim owes are lanes of clippy and
rustdoc only, after the tested sweeps. Those two phases select, narrow and
dedupe them like any sweep; the test phase, `prepare` and the coverage audit
never see them (see `[[lint_sweep]]` in `brokkr man sweeps`). An ad-hoc
`--features` run takes none.

When `-p` excludes every applicable sweep of an enabled phase, that phase
refuses as a failed run instead of reading green. The test phase refuses
before `prepare`, so a run that will test nothing compiles nothing for it.
The `--json` trailer's `sweeps` lists the sweeps some phase started work on.
## Feature projection

A `[[check]]` entry's `features` are written against the entry's own
selection. An entry over `vm`, `strategy` and `harness` may list
`vm/opcode-counts` and `strategy/hotpath`, and the full run is fine because
some selected package routes every token. A run that selects fewer packages
differs: cargo rejects every token none of its packages routes ("the package
does not contain these features").

Three things narrow a run below the entry's selection:

- **`-p`** on `check` or `brokkr clippy`, or `brokkr test`'s resolved package
  (which always overrides)
- **Package mode** (`feature_unification = "package"`), which runs one cargo
  resolution per package with no `-p` at all
- **Support builds**: each `build_packages` pre-build selects its one package

So brokkr projects the entry's tokens onto the packages each request selects.
For a token and the run's packages P:

- A bare `f` is routed by a package that has a feature `f`, or an optional
  dependency (any kind, any target) named `f`, its implicit feature.
- `x/f` or `x?/f` is routed by a package that declares a dependency (any
  kind, any target) named `x`, or by the package `x` itself when it has `f` by
  the bare rule. The dependency name is the `rename` when there is one, else
  the manifest spelling, hyphens kept.
- Anything else (`dep:x`, more than one `/`, an empty half) is malformed and
  routes nowhere.

A token some package in P routes is kept. A token no package in P routes, but
some member of the entry's domain does, is dropped. A token nothing in the
domain routes is kept, so cargo rejects it with its own message, as the full
run does. Malformed tokens are always kept.

The domain is the entry's own selection for the phase, resolved to workspace
members before any `-p` or per-package split. That is its `packages`, or
every member minus `test_exclude_packages`, or the workspace's default
members. A support build's domain adds every `build_packages` entry.
`--all-features` and `--no-default-features` are flags, not tokens, and pass
through unchanged. When every token is dropped, brokkr omits `--features`.

A package that depends on the others keeps their qualified tokens. Under
`-p harness` (depending on `vm` and `strategy`) the run above keeps both
tokens. Under `-p strategy` (depending on `vm`) it keeps `vm/opcode-counts`
and `strategy/hotpath` but not a `runner/...` token.

brokkr states every drop once per lane, before any phase runs, in the run log
and in `check --audit`'s plan, not on the console. `brokkr test` logs them the
same way:

```text
sweep piners, -p piners-strategy: dropped piners-runner/trace (applies to piners-runner, piners-harness only)
sweep bin, build piners-runner: dropped piners-strategy/hotpath (applies to piners-strategy only)
```

The sweep's shape line, a failing command and the parallel lane's repro line
all render the projected features, not the configured list.

brokkr doesn't project features the invocation names itself: `check
--features`, `--no-default-features` and `brokkr clippy`'s ad-hoc flags. You
asked cargo for exactly those, and a token cargo rejects is yours to see.
`brokkr clippy --sweep NAME` replays an entry, so it projects that entry's
features like any gate sweep's. That is an identity over the entry's own
selection, but a package-mode entry still projects per package.

The routing reads one `cargo metadata --no-deps` per invocation (each
member's `features` and declared dependencies), fetched without feature flags
and cached. brokkr reads it only when some request with tokens selects
something other than its domain, so an ordinary full run pays nothing.

A lane's own test runs project against the entry's selection only. A token
that only a `build_packages` package routes is kept there, and cargo rejects
it on the lane's run. Put support-only features on a sweep whose selection
includes that package.

## Selected entries

`brokkr check --textlint NAME` and `brokkr check --script NAME` run only the
named `[[textlint]]` rules and `[[script_check]]` entries, skipping every
other phase. Both are repeatable or comma-separated, and they combine. A
named script check runs whatever its `stage`, since stages only place checks
around phases this run skips. It also runs whatever its `when`: naming an
entry is an explicit request, so a `gate` or `off` entry runs too.

This is for iterating on one failing gate, where a full run would re-run
clippy before reaching it each time. It is never a gate:

- It prints `selected entries only - not a gate` up front.
- Clap refuses it beside `--gate`, `--profile`, `-p`, `--features`,
  `--no-default-features`, `--force-rust`, `--json`, `--timings`,
  `--commands`, `--fix-gremlins` and forwarded test args. `--json` is refused
  because its trailer is a verdict a machine could read as a pass.
- A name that matches no entry is an error listing the known names, so a typo
  can't print a green run.
- A failing selection exits 1 with no closing `[error]` echo of the phases'
  own reports, which printed in full. An error that isn't a phase report,
  such as an IO error from the scan, is its only diagnostic and prints.

It selects what runs for you, never what runs for a gate. That's also why no
per-entry "`-p` applies to me" key exists: it would let a narrowed run skip a
workspace-wide gate.

## Audit

`brokkr check --audit` explains the check this invocation would run, then
reports configuration findings. No phase runs, nothing is built, and no file
content is scanned. Planning reads what a run's planning reads, under the
lock, so the toolchain and rustflags policy apply:

- `cargo metadata --no-deps`
- git status, for the markdown-only shortcut
- the cargo config chain

The audit takes the flags that shape a run (`--profile`, `--gate`,
`--features`, `--no-default-features`, `-p`, `--force-rust`, forwarded
`-- ARGS`) and explains that run. Clap refuses it beside the flags that act on
one: `--json`, `--fix-gremlins`, `--timings`, `--commands`, `--textlint` and
`--script`.

### Plan

The run and the audit share one planner, so the audit can't describe a run
the check wouldn't make. `check plan: profile NAME (certifies ...)` heads the
plan, followed by:

- Every line the run would announce before its first phase, in order. That
  includes the run-log-only ones (profile header, lint lanes, script checks
  and lint sweeps ruled out by `when`, serial-build reasons), the
  markdown-only and skipped-phase lines, derived or quarantined doctest
  carriers, and the `-p` package-rule and feature-drop announcements.
- One `lane LABEL (tested | lint only): configured ...` line per lane: its
  packages (or the `test_exclude_packages` workspace, or the default
  members), features, target, pinned unification and support builds. This is
  the configured shape; a narrowed or per-package resolution can have less,
  and the feature-drop lines name every drop.
- One `phase NAME: ...` line per phase: `skipped` (profile `skip_phases` or
  the markdown-only tree), `not configured`, `none owed` (every entry ruled
  out by its `when`), `refuses` (a `-p` that reaches no lane), `not run` (with
  the reason), or `planned` with its eligible-lane, rule or check count.
  `script_check` also names the post-test checks a skipped test phase takes
  with it. Planned means planned: an earlier failure can still end a run
  first.

Then comes `textlint: N rules (files selected by path)` and one count per
rule. The count is the files the rule's `paths` select and its `exclude`
leaves, out of the scanner's own list (tracked plus untracked files git
doesn't ignore). Paths observed absent from disk, such as a deletion not yet
staged, are left out. It's a path count, not a scan count: `test_code`,
`only_if_file_matches` and binary content can still keep a selected file from
being matched.

### Findings

The audit prints each finding as a warning:

- A `paths`, `exclude` or `exempt` glob in `[header]`, `[[textlint]]` or
  `[manifest]` (`shape_exclude` included) that selects no present file. It
  uses the same list as the counts, so a path deleted but still listed by git
  keeps no glob live.
- An exclusion that excuses every file its selector picked, and a
  `[manifest]` scope that leaves no manifest. A `[[textlint]]` finding the
  textlint phase also fails on, because the rule sets `require_live_paths`,
  ends `(fails textlint when run: require_live_paths)`. Whether the phase runs
  is the plan's `phase textlint` line.
- A `[gremlins].include` glob that matches no present file, and a
  `[gremlins].exclude` entry that matches no scannable file (built-in
  extension or `include`) or removes every one. Under
  `[gremlins].disable = true` these are all skipped.
- A `[[check]]` entry no `[test.profiles]` entry references through `sweeps`,
  `extends` or `lanes`, reported only where profiles exist. It isn't dead:
  without a `default_profile` a bare check and `brokkr test` select every
  entry, and `brokkr clippy --sweep` replays any entry.
- Every `[[dependency_rule]]` `from` name that isn't a workspace package, and
  every `except` name that `from` doesn't select. The audit lists all of them,
  where the dependency-rules phase refuses on the first. Names in `forbid` and
  `allow` are never findings, since absence is never a violation.
- A `[[quarantine]]` `package` that isn't a workspace package, so the entry
  justifies nothing.
- A `[lints] allow_exact` entry whose path is absent under the build root (a
  dangling symlink counts as absent). This is an absence, not a staleness
  verdict: a generated file may be missing until a build makes it.
- `check plan refused: ...` when planning itself refuses (an unknown profile,
  a `-p` under a complete claim). The plan lines stated before the refusal
  still print, under `check plan (incomplete - planning refused)`.

A `cargo metadata` failure is a finding wherever it stops something (the
plan, the package selectors, or both), and the other findings still print. A
stop (Ctrl-C, `brokkr kill`) ends the audit as it ends a run.

The audit can't report two things, because no build-free answer exists:

- A `[[quarantine]]` pattern justifying no pair, or a sited allow that
  suppressed nothing. Both need a run's enumeration or diagnostics, and a
  complete run reports them.
- The `test_code` classifier's notes and refusals. They need the Rust sources
  parsed, and every textlint run already prints them.

An ordinary check prints none of the findings. A user-wide rule legitimately
names paths most trees lack, and an exclusion may reserve a path that's absent
today, so a warning on every run would be noise. The one opt-in exception is a
`[[textlint]]` rule with `require_live_paths`. That rule fails the textlint
phase on its dead `paths` globs and on an `exclude` that excuses everything,
judged by the same code as the audit. The audit ends with `audit: N findings`
and exits 0 whatever it finds.

## Markdown-only shortcut

In a git repo where everything uncommitted is markdown, `check` runs the
`gremlins`, `textlint`, `bench` and `script_check` phases and nothing else.
Clippy and the tests would re-prove what the last full run established on the
same code. `bench` stays because a bench's `script_file` may sit beside an
included config file outside the checkout, where the tree's status can't
vouch for it.

Markdown never counts as code, including markdown a Rust source compiles in
(brokkr's own `include_str!`'d `man` pages, a crate's `#![doc =
include_str!("README.md")]` doctests). This is deliberate: a doctest or
rustdoc break from a docs edit surfaces on the next full run, and
`--force-rust` checks the build on demand.

`post-test` script checks are the exception inside `script_check`. They judge
a test phase, and with none having run they're skipped and counted on a
`script-check: N post-test checks skipped` line, as under a profile that skips
`test`. A `when = "gate"` script check never runs under the shortcut, since
every profile waives it. On a markdown-only tree it runs when a complete
profile does, with the rest of the full run.

brokkr classifies the tree from `git status --porcelain=v1 -z --no-renames
--untracked-files=all`, the same parse that supplies unstaged diagnostic
priority. Staged, unstaged and untracked-not-ignored paths all count, and
`.md` and `.markdown` are prose. A rename record's origin path counts too:
`git mv notes.md src/lib.rs` isn't a documentation edit. Only one of four
outcomes shortens the run:

| Outcome | Meaning |
|---|---|
| `Unknown` | not a git repo, or git couldn't be asked: full run |
| `Clean` | nothing uncommitted: full run, since a clean tree is the state a complete check is for |
| `ProseOnly` | markdown and nothing else: shortened |
| `Code` | anything else: full run |

These waive the shortcut:

- `--force-rust`, `--gate`, and any `--profile` (and so any `certifies`
  claim)
- `--features`, `--no-default-features` and `-p`
- trailing `-- ARGS`

A shortcut that could quietly skip the certifying run would be worse than
none: `--gate` has to mean the same thing on every tree.

A shortened run says so twice, because the announcement has scrolled away by
the time you read the verdict. Up front it prints `markdown-only tree:
running gremlins, textlint, bench, script_check (--force-rust to check the
build too)`, and at the end `check passed (markdown only - build phases
skipped)`. The `--json` trailer has it as `scope: "prose_only"`.

## gremlins

The gremlins phase runs first and fails the check on any banned Unicode
character. It scans tracked and untracked-not-gitignored files, so new plan
docs are caught before staging:

- files with the extensions `.rs`, `.toml`, `.md`, `.markdown`, `.js`, `.sh`,
  `.py`, `.yml` and `.yaml`, matched case-insensitively
- any file a `[gremlins].include` glob names

The banned set covers:

- invisible and zero-width characters
- non-breaking spaces
- bidi overrides
- em and en dashes
- typographic quotes
- emoji and pictographs: Misc Symbols, Dingbats, the emoji planes, and emoji
  variation selectors

The Arrows block (`U+2190..=21FF`) and the box-drawing and geometric shapes
(`U+2500..=25FF`) are deliberately spared: comments, formatter output and
tree or table rendering use them legitimately.

`--fix-gremlins` rewrites every banned character in place with its ASCII
equivalent before the scan runs, so the check then finds zero and passes.
Zero-width, bidi and emoji characters have no equivalent and are deleted.
`[gremlins]` can include further files (an extensionless `LICENSE`), exclude
directories (vendored material) and adjust the banned set; see
`brokkr man conventions gremlins`.

## header

The header phase runs next, only when a `[header]` section is present. A file
matching `[header].paths` (minus `exempt`) must contain `[header].pattern`,
with `{year}` expanded to the current UTC year. The byte substring may occur
anywhere in the file; the phase doesn't check its position. Missing text or a
stale year fails.

## textlint

The textlint phase runs next, only when `[[textlint]]` rules exist. Each rule
forbids a regex on lines of the files it selects. The keys (`allow_marker`,
`except`, `region`, `skip_after`, `test_code`, `must_scan`,
`require_live_paths`, `join_wrapped_use`, ...) are in
`brokkr man conventions textlint`. When a rule with a `test_code` scope
selects any tracked `.rs` file, the phase runs `cargo metadata --no-deps`
once to classify the workspace's test code before scanning.

### Context windows

A rule may also have context-window gates: `except_above`, `except_below`,
`require_above` and `require_below`, each `{ lines = N, pattern = "..." }`.
A gate filters a match by the raw physical lines around it. All four behave
the same: the match is suppressed if and only if `pattern` is found within
`lines` lines in that direction, excluding the match line and clamped at the
file edges. The names only document intent: `except_above` reads for a
preceding `#[cfg(...)]` exemption, `require_below` for a required token like
`biased;` after a `tokio::select!`.

Any matching window suppresses the violation; it remains only when every
configured window is clear. Windows read raw text, with no region masking and
no `use`-joining. Since the test is per line, write context patterns that
tolerate fragments (match `madsim`, not a full single-line attribute), so a
rustfmt-wrapped `#[cfg(...)]` still suppresses.

brokkr caps reported source excerpts at 200 characters, with an ellipsis when
the matched line is longer; matching still uses the full line.

### Coverage line

A clean run reports what it covered, as `textlint (21 rules, 812 files)` on
the conventions line, matching the `dependency rules` fragment. The file
count is files at least one rule applied to, not the tracked-file total, so
it describes the corpus actually scanned.

A rule whose `paths` glob has stopped matching anything still passes, but a
shrinking count gives it away. Two keys make it fail instead:

- `require_live_paths` fails on the dead glob. The failure has no file or
  line: it prints as `[name] require_live_paths: ...` ahead of the file
  violations, and counts among them.
- `must_scan` fails when one of its named sentinel files goes unscanned.

When the test-code classifier refuses or can't run, the phase fails with its
error after printing every finding it could make without it. Only the pairs
the classifier had to decide are withheld (see
`brokkr man conventions textlint`). An interruption, or a file the scan
itself fails to read, ends the phase at once and drops what was found. A file
the classifier fails to read is one of its refusals, so the classifier rule
applies to it.

## manifest

The manifest phase runs next, only when a `[manifest]` section enables a
check; it's off by default. It parses each `Cargo.toml` matching
`[manifest].paths` (minus `exclude`) with `toml_edit` and enforces the
configured manifest conventions.

When `sort_dependencies` is enabled, dependency keys must be sorted within
each blank-line group. `[dependencies.NAME]` dotted sections, which TOML
forces after the inline table, are their own group and never ordered against
it.

`shape_exclude` globs excuse a manifest from the structural checks only,
while still sort-checking it. The structural checks are section, crate-type
and package-field order, `[lints] workspace`, and bin and example flags: the
same set a `cargo-fuzz = true` stub skips. `exclude` skips the file entirely.

## bench

The bench phase runs next, only when the config declares `[bench.*]` script
benches or sets `bench_scripts` or `bench_tests`. It reads every bench's Lua
(the inline `script`, or the `script_file` from disk), compiles it with
dellingr and resolves its global names without running any of it. That
catches:

- syntax errors and unreadable files
- a read of a global nothing defines, such as a misspelt builtin or helper
- a script that never assigns `bench`

A script that reads `_G` gets a warning that its names went unchecked. What a
name check can and can't see is in
`brokkr man bench-config validating-a-script`. Every bench is checked. Each
finding names the bench, the file to open and the line, and a clean run
counts the scripts on the conventions line (`bench (3 scripts)`).

With `bench_scripts` set, the phase also refuses every bench whose script is
inline or whose `script_file` lies outside that directory. It refuses every
`.lua` file under it that no bench registers, apart from a module, a
contract-test file or an exemption. Each of these findings names the bench or
the file, without a line (see `brokkr man bench-config bench_scripts`).

With `bench_tests` set, it runs every contract-test file under that directory
against virtual hosts, with no build, data or child process. Each failed case
is a finding naming the file and the case (see
`brokkr man bench contract-tests`).

The phase also runs on a markdown-only tree. A partial profile can skip it as
`bench` in `skip_phases`. The schema is in `brokkr man bench-config`.

## script_check

The script_check phase runs next, only when `[[script_check]]` entries exist.
This is the default `pre-clippy` stage. An entry can name `pre-test` or
`post-test` to run at that point in the pipeline, or `background` to run
alongside the phases up to the prepare builds (see `stage` in
`brokkr man conventions script_check`).

An entry's `when` decides which runs include it, before any stage runs: every
check by default, or `gate` for complete-claim runs only. Entries left out
are named in the run log and not counted on the stage's line.

### Pass rule

Each entry runs `command` via `sh -c`, so pipes, redirects and env expansion
work, with the code tree as cwd. It passes if and only if the captured output
matches `expect`. Asserting on a success sentinel rather than the exit code
is the point: it catches a check silently stubbed to `exit 0`, because the
script must prove it ran to completion by printing the sentinel. brokkr
ignores the exit code; only a spawn failure is a hard error.

Each entry runs in its own process group under a deadline: whatever is left
of the `script_check` phase ceiling, less ten seconds for the report. brokkr
kills a hung script, pipeline and all, and fails that entry
(`killed after ...`, with the output so far) instead of letting the phase
watchdog end the whole run. A killed entry never passes, whatever it printed
first.

brokkr retains at most 1 MiB per stream. The entry fails if a selected stream
exceeds that bound, or if a child keeps its output pipe open past the drain
grace, because capture can't prove the complete stream matched.

### Reporting

Every entry runs, with no fail-fast within the phase, so one `brokkr check`
surfaces all broken gates. Each failure prints its retained stdout and stderr,
rendered against the entry's declared `diagnostics` shape. The keys
(`match`, `stream`, `stage`, `diagnostics`, `when`, `note`) and what each
failure view prints are in `brokkr man conventions script_check`.

A clean stage reports once rather than per entry: `script-check (21 checks)`
on the conventions line for the `pre-clippy` stage, and its own
`script-check: ok (2 checks)` line for a later stage. The count keeps the line
falsifiable: a stage that quietly stopped running its checks shows a
shrinking number. A partly failing stage prints `script-check: M of N ok`
above the failure block.

An entry that declares a `note` prefix is the exception. When it passes, its
lines with that prefix print as `script-check: NAME: LINE`, so a script can
say it skipped an optional part without failing.

For `cargo doc`, prefer the native rustdoc phase (see "rustdoc") over a
script check: it reads cargo's JSON, so each diagnostic is one line with its
lint code.

`post-test` entries are skipped when the test phase failed. The test phase
fails fast, so its later lanes never ran and a sentinel gate has no
partial-run reading. The coverage audit does run there: it reconciles the
plan against the journal, which a failed run has as surely as a green one.
Every stage shares the one `script_check` phase name for `skip_phases` and
the JSON `failed_phase`; the output names the failing entry regardless.

## dependency_rules

The dependency_rules phase runs next, only when `[[dependency_rule]]` entries
exist in `brokkr.toml`; without them brokkr skips it silently. It reads
`cargo metadata --no-deps` and fails on configured direct-dependency boundary
violations: `from = "app"` with `forbid = "db"` rejects `app -> db`.

A rule can scope the forbidden match by dependency `kinds` (`normal`, `dev`,
`build`; default all) and `optional` (`optional = false` requires a dep to be
optional). That expresses manifest conventions like "tokio only as a
dev-dependency". A rule with `allow` instead of `forbid` inverts the
polarity: every in-scope direct dependency not on the list is a violation,
which keeps a leaf boundary closed as new crates appear.

## publish_cycle

The publish_cycle phase is the last convention phase, after
`dependency_rules`. It refuses a dependency cycle among publishable workspace
members. `cargo build` resolves the whole workspace at once and accepts a
cycle, so every clippy sweep and test lane stays green. `cargo publish`
uploads one crate at a time and needs each dependency in the published
manifest to already exist on the registry, so a cycle has no valid
publication order and fails at release time.

The phase needs no config. A publication cycle is derivable from the
manifests alone, is never intentional, and no other phase sees it. It costs
one `cargo metadata --format-version 1 --no-deps`, so it sits in the
always-on tier rather than in a command someone has to remember.

It ignores the CLI `-p` scope: publication order is a property of the whole
workspace, and a cycle a narrowed run hid would reach master.
`brokkr check -p one-crate` still catches it.

The analysis is `brokkr deps`' `publish_cycle` phase, the same function and
renderer, so the two commands can't disagree about a tree. That includes its
two exclusions:

- A version-less dev-dependency is stripped by cargo on publish, so it can't
  close a cycle. One that names a version does, and the finding names it,
  since deleting the `version` key usually fixes things without restructuring.
- `publish = false` members are excluded outright.

See `brokkr man deps` for the full rationale.

A clean tree prints `publish cycle: ok`, unlike `brokkr deps`, which is
silent there: a check phase that printed nothing would be indistinguishable
from one that didn't run. A finding fails the check with the loop rendered as
a chain:

```text
[error]   publish cycle: 1 cargo publication cycle
            nautilus-execution -[dev]-> nautilus-testkit -> nautilus-trading -> nautilus-execution
              dev-dependency nautilus-execution -> nautilus-testkit names a version, so cargo
              publish keeps it; dropping the version key leaves a path-only dev-dep that publish
              strips
              that fix only helps a consumer that keys on the published manifest - a release
              planner ordering every `path =` edge regardless of kind still sees this one, and
              needs the dependency removed outright
```

Every finding closes with a caveat line: the list may be partial, because
cycles sharing a member can surface one at a time (see `brokkr man deps` for
why). Any cycle fails the check, so gating is unaffected. When you use the
output to scope a fix, re-run after each one rather than trusting a single
report as the complete inventory. The phase points at `brokkr deps` for that.
A partial profile can skip the phase as `publish_cycle` in `skip_phases`.

## clippy

The clippy phase always invokes cargo with `--message-format=json` and passes
the resulting diagnostics through ownership, filtering, sorting,
deduplication and rendering. Cargo's pretty-printed text annotates only the
first occurrence of a lint per crate, which is why brokkr reads the JSON.
Cargo's own manifest lints are the one exception: they reach stderr only, as
text, and brokkr parses them into the same model (see "clippy manifest
lints").

The invocation is `cargo clippy --keep-going --all-targets
--message-format=json SWEEP-FEATURES -- --cap-lints=warn`. The last two flags
make one run surface every lint across a whole workspace, instead of one
error per run on a large multi-crate graph:

- `--cap-lints=warn` caps every lint at warn level, so a deny-level lint no
  longer aborts its crate's compile. The crate still produces its `.rmeta`,
  so every crate downstream of a linty one is checked too, and the whole
  dependency graph completes in one pass. Genuine non-lint compile errors
  still fail the crate, and `--keep-going` keeps checking the independent
  branches of the graph rather than stopping at the first failure.
- A capped lint lets cargo exit 0, so brokkr decides pass or fail, not
  cargo's exit status. Any clippy diagnostic fails the check, except a
  dependency's warning. brokkr reports a capped `warning` as the error it is,
  in the count and the header; it keeps clippy's `warning:` wording in the
  underlying JSON, and no view shows that wording. Only messages at `error` or
  `warning` level are diagnostics: rustc's spanless `failure-note` ("For more
  information about this error, try `rustc --explain`") isn't.

### Concurrency

The phase runs one cargo per build shape, or one per package under package
mode. brokkr groups these runs by the target directory they write: the shared
one, or a sweep's isolated `unify-*` or `rustflags-*` dir. Runs sharing a
directory go one after another in configured order, since cargo would only
queue them on its build-dir lock; the groups run concurrently. Every run is
made and the verdict is decided once all are in, as for a serial phase, so a
lint found early in one group never cuts another group's runs short. The
rustdoc phase groups the same way.

Three things make it one group:

- `[test] serial_builds`
- a cargo build dir configured for any lane these two phases run, a
  `[[lint_sweep]]` `env` included
- Cargo config brokkr can't fully inspect; the Cargo invocation then reports
  the unreadable or invalid config files (see `brokkr man sweeps`)

### Manifest lints

Cargo's own lints (`[lints.cargo]` in `Cargo.toml`: `unused_dependencies`,
`unused_workspace_dependencies`, ...) never reach the JSON stream; cargo
prints them to stderr as text even under `--message-format=json`. The clippy
phase reads them from there and lists them beside clippy's, under the same
any-diagnostic-fails rule:

```text
error[cargo::unused_dependencies] crates/t/Cargo.toml:25:1 unused dependency `anyhow`
```

brokkr takes a stderr block for a manifest lint only when its `-->` arrow
points at a `Cargo.toml` and it has a lint identity. The arrow alone isn't
enough: cargo's ordinary manifest errors (a bad TOML string, an invalid
version) have the same arrow, and they stay in the captured stream, where
their excerpt explains them.

Cargo names the lint (`` `cargo::unused_dependencies` is set to `warn` ``)
only on its first hit per manifest. Another hit takes the code of a named
block in the same manifest with the same message shape (its backticked names
emptied), and only when that shape names exactly one lint there. An arrow
with no `line:col`, cargo's fallback when it has no span, still locates the
lint by manifest alone.

Test and support builds also read their stderr manifest lints through the
same owner index, so warnings from manifests outside the workspace can be
omitted. Rustdoc doesn't read them, since clippy already reports the same
cargo warning.

Stderr text names no package, so brokkr attributes each lint by the
`Cargo.toml` it names. Cargo prints that path relative to the workspace root
whatever directory it ran in, so a run from a member's parent directory still
prints `crates/a/Cargo.toml`. brokkr resolves it against `cargo metadata`'s
`workspace_root` and looks it up among the members' manifests:

- **A member's manifest** gets that member's package id and follows the run's
  selection like any other diagnostic: `check -p a` reports `a`'s manifest
  and no other member's.
- **The workspace root's `Cargo.toml`** is reported by every run, whatever it
  selected. It holds the workspace's own tables, which
  `unused_workspace_dependencies` is about.
- **Any other manifest** is a dependency's. Cargo runs its manifest-parsing
  lints over every path-sourced package it loads, members or not: a sibling
  checkout, a `[patch]`ed or `exclude`d crate. Its warning, like any
  dependency's, doesn't fail the gate; an error fails whoever owns the
  manifest.

To allow one, use the project's own `[lints.cargo]` in `Cargo.toml`, or
brokkr's `[lints] allow` or `allow_exact`
(`cargo::unused_dependencies@crates/t/Cargo.toml`). brokkr applies a `cargo::`
allow where it ingests the diagnostic, never as a `-A` flag: rustc knows no
`cargo` lint tool, so the flag would itself raise a diagnostic. For the same
reason brokkr leaves `cargo::` names out of the test phase's rustflags.

### allow

The `[lints]` section's `allow` list (`allow = ["clippy::unused_async",
...]`) appends `-A LINT` after `--cap-lints=warn` on every sweep and on
`brokkr clippy`. The listed lints never reach the diagnostic stream, so the
any-diagnostic-fails rule needs no carve-outs. `[clippy]` is an older
spelling of `[lints]`; brokkr unions the two.

This is the escape hatch for driving a foreign checkout under
`disable_toolchain`. brokkr lints with the host's newer clippy, which
surfaces lints the project's own pinned-toolchain CI can't see and its code
can't be expected to satisfy.

The phase announces the allowed lints up front (`clippy: allowing
clippy::unused_async ([lints] allow)`), under `--commands` and in the run
log, so a narrowed gate never reads as a full one. The `-A` flags appear in
the reprinted failing command. Entries must be bare lint names:
`clippy::`-qualified, `cargo::`-qualified, or plain rustc names. The parser
rejects flags. A `cargo::` entry doesn't take the `-A` route; it acts at
ingestion (see "clippy manifest lints").

**`allow` has a known limit.** The injected `-A` flags act at the CLI lint
level, and a source site with its own lint-level attribute can override them.
The observed shape, on clippy 1.98 and nautilus_trader a930c8afe3: a function
with `#[expect(clippy::unused_async, ...)]` still fired the sibling lint
`clippy::unused_async_trait_impl` at error severity, with both
`-A clippy::unused_async_trait_impl` and `--cap-lints=warn` on the command
line. The expectation machinery's diagnostic bypassed both, and the same lint
was suppressed fine at attribute-free sites.

The best reading is a clippy expectation-machinery interaction, not a brokkr
defect. In practice, `[lints] allow` can't be relied on to silence a lint at a
site holding an `#[expect]` for a sibling lint emitted by the same pass. A
minimal upstream repro sketch: an inherent `async fn` with a tail expression
and no `.await`, `#[expect(clippy::unused_async)]`, compiled with
`-A clippy::unused_async_trait_impl` on clippy 1.98.

### allow_exact

`[lints] allow_exact` is the remedy for that shape. Its `"lint@path"` entries
are suppressed on brokkr's side of the pipe instead of the compiler's. brokkr
drops a matching diagnostic (same lint, with or without the `clippy::`
qualifier, and same build-root-relative file) at JSON ingestion. That happens
after clippy has spoken and before the any-diagnostic-fails decision, so no
lint-level attribute at the site can defeat it.

It's deliberately narrow where `allow` is broad:

- one lint in one file, never workspace-wide
- every occurrence in that file, by design, since line numbers drift with
  unrelated edits
- no `-A` injected, so other sites of the same lint keep failing the check

The path half must match the file exactly as clippy reports it, relative to
the tree cargo compiles in. Copy it from the failing diagnostic's location.
A suppressed diagnostic is dropped from both the pass-or-fail decision and the
output; nothing shows it.

The entries' distinct lints are named on the verdict line
(`allow_exact: clippy::assert_is_empty, deprecated`). The scope is stated
here, not on that line. An entry is sited per file in the diagnostic phases
(clippy and rustdoc). The compiling phases (test, coverage enumeration,
member-check) have no per-file mechanism, so there it applies build-wide
through rustflags. `cargo::` entries never reach a build and are marked
`(sited)` on the verdict.

The per-lint file-count summary (`clippy: allowing clippy::assert_is_empty
(59 files), deprecated (3 files) ([lints] allow_exact)`) goes to the run log,
and prints under `brokkr clippy` or `--commands`. Under `brokkr clippy` the
blanket `clippy: allowing ... ([lints] allow)` line is log-only, since its
printed command already shows the `-A` flags. Naming the lints keeps a
narrowed gate from reading as a full one. The paths are already in
`brokkr.toml`, and one line per entry would bury the rest of the run once a
project has more than a handful.

An entry that suppressed nothing across the run draws a
`suppressed nothing (stale entry?)` warning: upstream fixed the site or the
file moved, so delete or re-site the entry. The notice fires only on unscoped
runs. A `-p`-narrowed run doesn't check an entry's file outside the selected
packages, so "suppressed nothing" there proves nothing. `brokkr clippy`
applies the same rule to its one probe shape, and stays silent unless that
shape is unscoped, lints every target (no `--lib`) and uses
`--all-features`.

Each phase judges only the entries it can produce: clippy the non-`rustdoc::`
ones, the rustdoc phase the `rustdoc::` ones. A project without `[rustdoc]`
never judges those.

## rustdoc

The rustdoc phase runs after clippy and before the `pre-test` script checks,
only when `brokkr.toml` has a `[rustdoc]` table (an empty one is enough).
Rustdoc is the one compiler pass neither clippy nor the tests run. Without
this phase a project's `rustdoc::` lints are declared and never evaluated,
and a doc link broken by a file split rots unseen.

The invocation is `cargo doc --no-deps --keep-going --message-format=json`.
brokkr adds `--document-private-items` when
`[rustdoc] document_private_items = true`, and the sweep's selection exactly
as clippy builds it: profile, feature unification, packages, features, env
and isolated target dir. Rustdoc resolves `cfg` like the build does, so
documenting under any other shape would judge doc comments on code that shape
never compiles.

Cargo hands a sweep's `rustflags` to rustc only, so brokkr also places their
`--cfg` and `--check-cfg` entries in the rustdocflags cargo reads (see
`rustflags` in `brokkr man sweeps`). A `--cfg` hook build documents its
hook-only items.

The phase runs once per distinct build shape, deduped and `-p`-intersected by
the same rules as clippy (see "Invocation selection") and through the same
loop, so the two phases can't disagree about what a sweep covers. A
`doc_only` sweep is not applicable
(`skipped (doctest carrier, no build shape of its own)`). It exists to run
doctests, so documenting it would re-report another sweep's diagnostics under
a second label. It can't always be deduped either, since a carrier that
inherits feature unification from `.cargo/config.toml` differs on argv from a
sibling that passes it.

On a nightly rustdoc that supports it, the phase lints without rendering
(`-Zunstable-options --check`), detected automatically; no key controls it.
The reported lints are the same.

Output goes through clippy's formatter: one
`error[LINT] FILE:LINE:COL MESSAGE` line per diagnostic, merged across
sweeps, nothing capped. A diagnostic is tagged with the sweeps that reported
it only when some sweep covering its package didn't, such as a link that
breaks only with a feature off.

Any diagnostic fails the phase, warnings included, as for clippy. Rustdoc's
lints are warn-by-default, so a gate failing only on `error` would pass every
broken link. Suppression happens at ingestion:

- `[lints] allow` drops a diagnostic whose lint code equals an entry exactly
  (`rustdoc::private_intra_doc_links`; group names match nothing). Prefer
  `Cargo.toml`'s `[lints.rustdoc]` for a rustdoc lint: brokkr's list also
  reaches the test phase's rustflags, and a new entry costs a full test
  rebuild.
- `[lints] allow_exact` drops `lint@path` sites. This phase judges the
  `rustdoc::` entries for staleness; clippy judges the rest.

A partial profile can skip it as `rustdoc` in `skip_phases`. The
markdown-only shortcut skips it, and it has a 5-minute ceiling like clippy.

## Exit codes

A profile may declare `certifies = "complete"` or `"partial"` (see
`brokkr man sweeps`). The claim decides the success word, the exit code and
which narrowing is permitted:

| Profile | Success line | Exit on success | Exit on failure |
|---|---|---|---|
| no `certifies` (legacy) | `check passed` | 0 | 1 |
| `certifies = "complete"` | `check complete` | 0 | 1 |
| `certifies = "partial"` | `check partial (...)` | 10 | 1 |

Partial's exit 10 is the point: `brokkr check && git commit` on a partial
profile fails closed, so a loop answer can't silently stand in for a gate
answer. The partial success line lists what was narrowed (skipped phases,
`-p` scope) and never contains the word `passed`. `skip_phases` on a partial
profile skips the named phases and announces them up front.

A `complete` profile rejects `-p` before anything compiles. A scoped build's
green isn't comparable to the full build's, because feature unification
changes with the package set. It rejects trailing `-- ...` test args the same
way: a libtest `--skip` or a cargo `--lib` narrows the real run but not the
plan, which brokkr builds from the sweeps' own filters.

The other exit codes:

- **2**: a clap usage error
- **124**: a time ceiling fired
- **130**: an interrupt

### Disk space warning

Before the verdict, check warns when a filesystem holding brokkr's own state
has under 10% free. That state is the project's `.brokkr` stores,
`~/.brokkr`, and the XDG data and cache directories. The warning names each
store on that filesystem and what shrinks it, or that nothing does, and never
changes the exit code.

It costs one `statvfs` per store root, probed at the nearest existing
ancestor of a root not yet made, and never walks a store; `brokkr env` lists
the sizes. The free-space gate every locked command passes already guards the
cargo target disk, at 5% (see `brokkr man clean`).

## Sweep log lines

Each sweep describes itself as `PHASE NAME: SHAPE` rather than its full cargo
command, in the run log and on failure only. A passing run never prints
these. The shape is a function of the config and the invocation, identical
run to run, so on green it has no news; what this invocation changed prints
on the `invocation:` line instead (see "Output"). brokkr writes the shape to
the run log with the full argv under it, and to the status line on a
terminal:

```text
[run]     profile tier1: 3 sweeps (default, ffi, live)
[run]     clippy default: default selection
[run]     cargo clippy --keep-going --all-targets ...
[run]     clippy ffi: 4 pkgs, +ffi
[run]     test default: workspace -2 pkgs, 14 skips, parallel
```

The package part of a shape is the phase's selection for that sweep (see
"Invocation selection"). The default sweep's clippy line above passes no
package flag, so it reads `default selection`, while its test line shows the
`test_exclude_packages` exclusion. A failing sweep prints its shape beside its
failing command.

Resolved facts the config doesn't spell out, such as
`feature_unification = "auto"` promoting a lane or the topology-derived
parallel budget, are machine- and tree-stable and stay log-only. They are
data for a cross-host comparison, not for reading a verdict.

The full command is about 90% profile boilerplate repeated per sweep. On
nautilus_trader the three `cargo test` lines are about 1,100 characters each,
of which about 900 are the same 14 `--skip` flags from the profile. What
varies is package scope and features, which is what the shape shows.

### Shape

The shape is `PACKAGE-SCOPE[, FEATURES][, rustflags ...][, TEST-BITS]`:

- **Package scope**: `workspace`, `N pkgs` (a `packages` list, emitted as
  `-p`), or `workspace -N pkgs` (`test_exclude_packages`; test phase only,
  since clippy stays workspace-wide).
- **Features**: read back out of the request's feature argv, so it can't
  drift from what cargo is handed: `all-features`, `no-default`, `+ffi,live`.
  This is the projection, not the configured list (see "Feature
  projection"). A lane-level line shows the tokens some run of the lane
  passes. A fragment that merely restates the sweep's name is dropped; the
  legacy no-`[[check]]` path names its synthesized sweep `all-features`.
- **`rustflags FLAGS (isolated target)`**: always part of the shape, because
  `rustflags` silently redirects the sweep to `target/rustflags-HASH`, and the
  log is where an unexplained full recompile gets explained.
- **Test-phase bits**: `N skips`, `include-ignored`, any `--test NAME`
  filters, and the lane (`serial` under the per-test watchdog, `parallel`
  otherwise).

### On failure

Failures always print the shape and the full command, as
`[error] PHASE NAME: SHAPE` and `[error] failing command: cargo ...`. When a
sweep fails, the copy-pasteable line is the most useful thing in the output.
This covers:

- clippy and rustdoc failures and test failures
- hung tests and parallel-sweep timeouts
- zero-test runs and `build_packages` pre-build failures
- any error leaving a test lane, the nextest lane's `cargo metadata` call
  included

A failure that has printed its full diagnosis (a blown time budget, a failed
pre-build, a lane that couldn't be prepared) ends without the closing summary
message. So every fact, including the package, sweep or test identity, must
appear in the printed block itself. `--commands` doesn't suppress any of
this: the line that flag streamed earlier is neither beside the failure nor
attributable among several runs.

### commands

`--commands` prints each cargo command on stdout before it runs: every
sweep's, the pre-builds', the enumeration's, member-check's and its probes',
fixture-lint's, and the `cargo metadata` invocations of the dependency-rule
and publish-cycle phases. brokkr streams them rather than collecting them, so
a run the watchdog kills still shows what it was running. The grouped green
lines don't change.

`brokkr clippy` ignores the flag and always prints its command, once and
after the run. It's the investigative runner, invoked to find out what a
given target shape does. A green run prints the cargo line. A failing one
prints the shape and `failing command:` pair above, also on spawn failures,
interrupts and deadlines, and doesn't stream the line first as well. Its
ad-hoc sweep is labelled `clippy`, so the pair reads `[error] clippy: SHAPE`
rather than `clippy clippy:`.

### Phase lines

Member-check's green line has only the run's counts and profile
(`member-check: ok (3 members, 3 libs, 2 bins, dev) in 8.1s`, or
`1 of 3 members, ...` under a `-p` that narrowed the set). The constant
explanation of the phase (each member resolved on its own like
`cargo install`, shared lockfile, no codegen) is log-only. Fixture-lint's
line names each fixture's variants, files, expectations and target-dir size.

The shim-fallback warning (`harness isolation unavailable (...)`) prints once
per sweep per run, not once per cargo resolution of a package-mode sweep.

## Sweep selection

The invocation decides the sweep set and the libtest filters:

| Invocation | Sweep set | Libtest filters |
|---|---|---|
| no `[[check]]`, no flags | one `--all-features` sweep (legacy default) | none |
| `[[check]]`, no `default_profile`, no flags | every `[[check]]` entry in declaration order | none |
| `[[check]]` + `default_profile = "tier1"`, no flags | the entries `tier1.sweeps` references | tier1's filters |
| `--profile tier1` | the entries `tier1.sweeps` references | tier1's filters |
| `--features X` (or `--no-default-features`) | one ad-hoc sweep, no `build_packages` | the resolved profile's filters (see below) |

Every row but the ad-hoc one also lints and documents the `[[lint_sweep]]`
entries its claim owes, after the sweeps above. They aren't sweeps of the
test phase.

### Ad-hoc runs

CLI features override sweep selection, not run shaping. Sweep selection
decides what cargo compiles. A profile's run-shaping keys decide which tests
run and how: `skip`, `only`, `tests`, `lib_tests`, `exclude_tests`,
`include_ignored`, `test_threads`, `isolation` and `env`. A test that can't
pass in-process can't pass in-process at any feature shape. So an ad-hoc run
inherits the shaping of the profile it would otherwise have used
(`--profile NAME`, else `[test] default_profile`).

It also inherits entry-level `env`, unioned across every `[[check]]` entry,
the same union `brokkr clippy`'s ad-hoc path uses, so the two commands
resolve one shape from one config. brokkr treats an entry `env` as a
build-affecting invariant, such as a var a build script reads or a codegen
toggle. A probe that dropped one could go green having compiled something
other than what the gate compiles. A key two entries set to different values
is a hard config error naming the key. Entry env overlays profile env on a
collision, as on non-ad-hoc sweeps.

brokkr can't tell a load-bearing entry `env` from an inert one, since that
would mean knowing what every `build.rs` reads, so it applies all of them.
This doesn't cover a project whose real invariant is a cargo feature rather
than an env var. There the shape follows feature resolution and so follows
the CLI scope, which no env union can restore. An entry `env` that nothing
reads is harmless but proves nothing; don't read its presence as evidence
that a scoped run matched the gate's shape.

The ad-hoc run still takes no `[[check]]` entry, so it inherits no
entry-level `features`, `test_exclude_packages` or `build_packages`. In
particular, `test_exclude_packages` isn't applied: an entry that excludes a
package from its test selection (one that would link libpython, say) will try
to link it here. Exclusions aren't unioned like `env`, because they're a
per-entry selection workaround rather than an invariant, and unioning them
would narrow what a scoped run tests while the rest of the run reads wider.
Prefer a profile when the entry's selection matters. A shape needing an
entry's exclusions and different features at once isn't expressible as any
union, and has no spelling.

A `lanes` profile shapes nothing of its own, since lanes have no run-shaping
fields. An ad-hoc run under one inherits no filters rather than silently
borrowing one lane's.

Every ad-hoc run names what it inherited, `--commands` included (that flag
adds the cargo lines and suppresses nothing):

```text
[run]     ad-hoc features: sweep selection overridden, run shaping from profile tier1
[run]     ad-hoc features: sweep selection overridden, no profile - no test filters applied
```

An ad-hoc run reports no profile in the header or the `--json` trailer,
because it claims no `certifies`. This line is the only statement of its
filters, and without it a red is indistinguishable from a code failure.

### Lanes

A profile with `lanes` resolves to the concatenation of its lanes' sweeps,
with lane-qualified labels (`tier1/default`, `serial/default`). The test phase
runs each lane's entry separately, since contradictory filter sets are the
point. The clippy and rustdoc phases dedupe sweeps that compile the same
thing and log `clippy LABEL: deduped`. The same thing means the same requests
(selections, projected features, support builds) and the same rustflags,
env, profile and effective unification (see "Invocation selection").
`profile` is in the key because `cfg(debug_assertions)` decides which code
exists: a dev and a release sweep of the same features present different lint
surfaces, so brokkr lints them separately.

### brokkr test

`brokkr test NAME` follows the same ladder, with these differences:

- It drops filters, since your `NAME` argument is the filter.
- It has no CLI ad-hoc path; the test runner doesn't accept `--features`.
- Sweeps that would execute identically for the resolved package fold into
  the first, since with filters dropped lane duplicates are identical runs. A
  sweep and its `doc_only` twin never fold, nor do two sweeps with different
  `build_packages` (see "Invocation selection").
- `--sweep` applies before that fold, and its labels under a lanes profile
  are the lane-qualified form.

Per-project orchestration blocks such as `[ratatoskr.harness]` aren't
`[[check]]` sweeps, and neither `brokkr check` nor `brokkr test` sees them.
They describe how to build a binary that ratatoskr's orchestration commands
(`service`, `mock-serve`, `sync`) spawn, with their own `package`, `features`
and `debug` fields. `[test.profiles]` may reference only `[[check]]` entries
in its `sweeps` list, never an orchestration block.
