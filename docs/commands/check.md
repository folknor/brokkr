# brokkr check + brokkr test

Both commands share one sweep and profile machinery. `check` is the full
validation pass (gremlins + header + textlint + manifest + script checks +
dependency rules + publish cycle + clippy + optional rustdoc + prepare + tests,
with coverage under a complete claim, and member-check and fixture-lint when
configured);
`test` runs the cargo tests matching one name against the same sweep set.

For the underlying config (`[[check]]`, `[[dependency_rule]]`, `[test]`
section, profiles) see `docs/brokkr.toml.md`. For how the pipeline is built -
selection, accounting, harness isolation, the guard protocol, the per-file
module map - see `reference/check-internals.md` and
`reference/architecture.md`.

This doc covers the command, its output, and the phases up to the tests, then
`certifies`, log lines and sweep selection. The rest lives in sibling topics:

- `brokkr man test-lanes` - the test phase and its lanes (serial, parallel,
  process-isolated, nextest, doctests), coverage, diagnostic continuation,
  feature unification, member-check, dead filters.
- `brokkr man conventions fixture_lint` - the fixture-lint phase.
- `brokkr man test` - `brokkr test`.
- `brokkr man hygiene` - time ceilings and the per-test cap, strays, the rustc
  guard.

## `brokkr check`

Gremlins + header + textlint + manifest + script checks + dependency rules +
publish cycle + clippy + optional rustdoc + prepare + tests + coverage under a
complete claim + member-check and fixture-lint when configured. Trailing args after
`brokkr check --` are split on a literal `--`: tokens before it go to
`cargo test` (e.g.
`brokkr check -- --test cli_sort` scopes to one test crate), tokens after go
to libtest after the default `--test-threads=1` (e.g.
`brokkr check -- -- --ignored`). With no second separator, every token is
cargo-level. The leading `--` is **required**: `check` takes no
`trailing_var_arg`, so an unrecognised brokkr flag is a parse error rather
than a token silently forwarded to `cargo test`. The test phase also fails on a successful `cargo test` that ran
zero tests (suites=0, or filters excluded everything) so a too-narrow
profile/filter combo can't silently green-light a check. The test phase
closes with one grouped line - `test: N passed (, M ignored / , K filtered
out when non-zero) (S sweeps; ...)` - so a green run always says how much it
ran (see "Output"). A unit that *legitimately* ran zero tests (nothing
filtered out - e.g. an all-doctest crate, since `--tests` excludes doctests)
still passes, but is named on that line as `no tests ran in <sweep>` rather
than vanishing into the total; on an explicit `-p <pkg>` spot-check that ran
nothing, an extra warning notes the green validated clippy, not tests. The
warning is scoped to the hand-typed `-p` path so a whole-workspace run never
nags.

Like every locked brokkr command, `check` and `test` acquire the global
per-user lock **blocking**: if another brokkr invocation (e.g. a bench run)
holds it, the command prints `[lock] waiting for the brokkr lock ...` and
waits until released, then proceeds - rather than failing with
`lock: already locked`. So a concurrent lock never produces an error to
handle; just let the command wait. The wait is the serialization working, not
machine congestion: the lock exists so brokkr commands never overlap, and the
`[lock] acquired - you now hold the global brokkr lock for the duration of
this command` line marks the point after which this command had the lock to
itself - the wait has no effect on any timing or result measured after it.
Both lines' wording is fixed on purpose: every terser revision was misread by
agents as a sign that other brokkr commands were contending for the machine. `brokkr lock` shows who holds the lock and what it is doing - asked
of the holder itself over its control socket, so it works across sandboxes.
When the holder recorded the same `CODEX_THREAD_ID` as the waiting command, the
wait says so: the holder is almost certainly the agent's own earlier command,
still running in another exec session. The one holder a command does not wait
for is its own ancestor - a brokkr started beneath a locked brokkr refuses at
once instead (see `brokkr man hygiene descendant`).

Flags:
- `-p/--package <PKG>` (repeatable) - scope every sweep's cargo invocation
  (clippy, rustdoc, test, member-check) to the named packages. The set
  **replaces** each sweep's own package selection - cargo unions selection
  flags, so composing `--workspace --exclude ...` with `--package` would
  silently un-scope the run. Per sweep the set is *intersected* with the
  sweep's scope: a package its `packages` list or (test phase only)
  `test_exclude_packages` rules out is dropped, a sweep keeping none is
  excluded from that phase (mirroring `brokkr test`'s SKIP) - so `-p a -p b`
  still reaches `a` in the sweep that admits it when `b` lives in another
  sweep. An excluded sweep builds nothing for that phase - its
  `build_packages` support binaries included. If every applicable sweep is
  excluded, the phase fails rather than reading as green (see "Invocation
  selection" for when that refusal fires). The `invocation:` line up front
  shows `-p <pkg> ...`; right after the phase announcements, when the sweeps'
  package rules narrow the selection, one `package rules narrow this
  selection` announcement groups the sweeps by what each phase made of them
  (`  vm, runner: excluded (not admitted - -p x is not in this sweep's
  packages list)`), naming the phases apart when they differ (`clippy:
  eligible | test: eligible with -p b (not admitted - ...)`). Per phase it
  reports the admission (the dropped packages) and the disposition -
  eligible, excluded, deduped (and onto which sweep), or not applicable. A
  phase the run skips is left out. Eligible means planned, never "will run":
  an earlier failure or a stop can still end the run first. The per-phase
  `skipped`/`dropped`/`deduped` lines go to the run log only (`brokkr
  clippy`, which has no announcement, still prints them). The `--json`
  summary carries a `package` field (comma-joined for a multi-package run).
  Rejected under a `certifies = "complete"` profile
- `--features` / `--no-default-features` - ad-hoc sweep, no `build_packages`.
  Overrides sweep *selection* only; the resolved profile's run shaping (skips,
  filters, thread policy) still applies - see "Sweep selection"
- `--profile <NAME>` - selects a `[test.profiles]` entry; conflicts with
  `--features` / `--no-default-features`
- `--gate` - run the profile named by `[test] gate_profile` (load-validated
  to certify "complete"). The stable pre-commit invocation. Conflicts with
  `--profile`, `--features`, `--no-default-features`, and `-p`. Trailing
  `-- ...` test args are rejected under any `complete` claim (see `certifies`)
- `--json` - append one machine-readable summary line (a JSON object) as the
  last line of stdout; human output, including warnings, remains on stdout
- `--fix-gremlins` - rewrite banned chars in place before scan
- `--commands` - print each cargo command before it runs (see the log-lines
  section)

Output:
- **A passing run prints one line per phase, not per sweep.** An LLM runs
  `check` many times a session, and every green line is paid for on every
  run, so a green run prints only what can change what its reader does next.
  A five-sweep run reads:
  ```
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
  The convention phases (gremlins through publish cycle) share one line with
  their counts; each build phase (clippy, rustdoc, prepare - a complete
  profile's line; any other run's goes to the log - test, coverage - which adds
  the accounting line - member-check and fixture-lint) prints one line with
  its wall time as it completes -
  streamed per phase, not rendered at the end, so a run the watchdog kills
  still shows which phases finished. A pre-test or post-test script-check
  stage keeps its own line. What a green run no longer prints - each sweep's
  shape and cargo argv, per-sweep counts and build/fan-out times, the
  parallel plan, the process-isolated roll-call, config-derived skips and
  build-shape dedupes - goes to the run log (below). Nothing is dropped, only
  moved.
- **What still prints on green**, because each changes what the verdict
  means: an `invocation:` line up front naming what this run changed about
  the configured gate (`-p`, a bounded preview of forwarded `-- ...` args,
  `RUSTFLAGS` /
  `CARGO_ENCODED_RUSTFLAGS` inherited from the environment - set-but-empty
  included, since cargo reads an empty one as a live source that shadows the
  config-file rustflags); the ad-hoc
  features, `skip_phases` and markdown-only announcements; `-p` drops and
  skips per sweep; warnings (below); zero-test units on the test line; and
  the verdict line's context - the profile, the sweeps that ran, and a bounded
  summary of `[lints]` suppressions (six names per group, with an omitted
  count for longer lists). The suppressions sit on the verdict rather than on a
  phase line because they narrow clippy (`-A`), rustdoc (dropped at
  ingestion) and every compiling phase (test, coverage, member-check,
  through rustflags) alike - except fixture-lint, which takes no allows; `allow_exact` lists its lint names only, its
  scope being a fixed fact documented under `[lints] allow_exact` below.
  `cargo::` lints are listed in a separate `(sited)` group, because brokkr
  never passes them to a build. A script that builds its own `-A` list
  from `[lints]` has to skip them too, or rustc fails every crate with E0602.
  The verdict prints on a red run too, so the context does.
- **Warnings print once.** Compiler and manifest warnings from a passing test
  sweep or support pre-build are parsed, owner-filtered, deduplicated by typed
  fields, and sorted before rendering. Identical rendered warning blocks are
  held until the phase ends and printed once with the sweeps that produced them
  (`every sweep` when it is all of them). A
  parallel plan in which two or more binaries each claimed the whole
  in-flight budget (budget above 1) warns, since those binaries could not
  overlap and the run otherwise reports success. A `[lints]` injection whose
  sink brokkr chose on a guess (see `src/rustflags.rs`: a `target.*.rustflags`
  selector it cannot evaluate) warns that the allows may be inert.
- **A failure prints what the green run left out.** Each failing sweep prints
  its shape (`test threaded/default: default selection, unification
  workspace, parallel`) and its `failing command:` beside its diagnostics. The
  shape's package part says what the run selected: `-p a -p b` for an
  override or a one-package selection, `N pkgs` for a sweep's own list,
  `workspace -N pkgs` for `test_exclude_packages`, and `default selection`
  when no package flag is passed - cargo's default members, which is not a
  claim about the whole workspace. Every failure
  site carries its own context - the diagnostic phases report after all their
  sweeps ran, and a parallel lane after its join, so no ambient "current
  sweep" could say which one a failure belongs to. A failure whose site
  printed its own findings (gremlins, lint diagnostics, the test failure
  list, a coverage worksheet) ends in `check failed in ...` alone; any other
  error - an IO error, a `cargo metadata` failure, a config refusal found at
  phase time, a spawn failure - prints its message on the line above the
  verdict, so a red run always names its cause. In code, the first kind is a
  `DevError::Reported`; everything else is printed by the summary.
- **A graceful `brokkr kill` (or Ctrl-C) ends a run as `check interrupted in
  ...`** and exits 130 with main's scratch cleanup, like every other locked
  command - not exit 1. `check` holds a `SigtermGuard` for its whole run: the
  handler kills the registered test groups at once, every runner that polls
  the flag returns, and no new test group is spawned. A second signal while
  the first is still being honoured takes the default action, so a wait that
  does not poll cannot swallow the interrupt. `--json` still emits its
  trailer, with verdict `"failed"`. `brokkr test` and `brokkr clippy` behave
  the same way.
- **The run log.** Each run attempts to write `.brokkr/check-logs/check-<ms>.log`
  (under the config dir), the newest ten `check-` logs kept (`brokkr test`,
  measured runs and `brokkr bench` write `test-`, `measure-` and `bench-`
  logs beside them, each kind pruned only against its own ten). Reserved before
  the lock and opened under it, before the compile-lease drain and toolchain
  narration. Waiting commands cannot open or prune logs; an open failure warns
  and preserves history. Names advance past the newest same-kind stamp after a
  clock rollback, so retention still prunes in creation order. If listing the
  directory fails, brokkr warns and skips retention for that run; a log it
  could not remove warns too, since a silent failure would quietly defeat the
  bound. The log keeps
  every line printed after opening, plus the narration a green run no longer
  prints: shapes, full cargo argv, the
  `phase <name>: started` line of every phase (followed by `phase <name>: not
  configured` when the phase had nothing to run, so an inert phase never reads
  as one that checked something), the
  `[lints]` sink, per-unit counts, build and fan-out times, the parallel plan
  and per-sweep slowest binary, the roll-call. Written and flushed as each
  event happens, so a watchdog-killed or panicked run keeps everything up to
  the point it stopped (the watchdog's own kill lines land in it too); ten
  are kept so that run survives the retries after it. This is where a
  cross-host comparison or a slow-build investigation reads from.
- **On a terminal**, one transient status line under the output names what is
  running (the phase, the sweep, the process-isolated test in flight). It is
  cleared from under every persistent line and at exit, and never drawn when
  stdout is a pipe or a file, so captured output is unchanged by it.
- Each diagnostic becomes one line, compilation noise stripped, passing
  tests aggregated. There is no unfiltered mode: rustc's rendered form -
  source excerpt, carets, `help:` spans - is not used by those renderers.
  A failed nextest pre-build prints the rendered compiler errors (including
  their notes and help) instead; warning-level compiler messages from that
  failed build are omitted.
  The one-line form is what keeps a thirty-error run readable, and a gate
  that can turn into a scroll gets read as one.
- Which diagnostics a phase shows, in every diagnostic phase (gremlins,
  header, textlint, manifest, dependency rules, clippy, rustdoc):
  1. **Everything is an error**, whatever level the tool reported it at,
     except a warning from a dependency - a package outside the workspace,
     told apart by the `package_id` cargo attaches to each message. Those
     neither fail the phase nor print. A dependency's *error* still fails.
  2. **Errors in files with unstaged changes come first**, then every other
     error. "Unstaged" is the working tree against the index (`git diff`)
     plus untracked, non-ignored files; staged and committed changes do not
     count. The error in the file being edited is the one to fix first.
     Clippy and rustdoc diagnostics are sorted by (lint code, file, line)
     within each group, so every hit of a rule clumps together.
  3. **Nothing is capped.** Every error prints, every time, and there is no
     flag that changes that. A cap plus an escape hatch means the default
     view of a bad run is a lie a reader has to know to re-ask; the number
     of errors is the reader's problem to scroll, not brokkr's to withhold.
     See `src/scope.rs`.
- `--json` appends one summary object as the **last line of stdout**, leaving
  the human output untouched, including its warnings (the old NDJSON per-event
  mode is gone; this is the result contract). Read the final line for the
  object; preceding lines remain human-readable output. Fields: `schema`
  (currently 2), `certifies` (the
  resolved profile's claim, `null` for unclaimed profiles), `verdict`
  (`"passed"`/`"complete"`/`"partial"`/`"failed"`), `profile` (the profile
  that drove sweep selection; `null` for ad-hoc and legacy runs), `sweeps`
  (the labels of the sweeps clippy/rustdoc or the test phase started work on
  - see "Invocation selection"), `package` (the CLI `-p` scope, `null` when the run was not
  scoped; multiple `-p` packages comma-joined), `failed_phase` (`null` on
  success, else one of `gremlins`/`header`/`textlint`/`manifest`/
  `script_check`/`dependency_rules`/`publish_cycle`/`clippy`/`rustdoc`/
  `prepare`/`test`/`coverage`/`member_check`/`fixture_lint`), `scope` (`"prose_only"`
  when the markdown-only shortcut skipped the build phases, `null` on a full
  run - without it a shortened `passed` reads as a full one), `termination`
  (how the run stopped, when something stopped it - `{kind, scope}`, see
  "Termination" under the `coverage` phase), `policy_coverage`,
  `execution_accounting` and `doctests` (complete profiles whose `prepare`
  phase ran, `null` otherwise - see the `coverage` phase),
  `diagnostic_continuation` (a failed run that left executions unresolved, any
  profile - see `brokkr man test-lanes diagnostic-continuation`), `elapsed_ms`. The object is
  versioned and additive within a version: fields are only ever added,
  consumers must tolerate unknown fields, and a bump is reserved for renames
  or semantic changes. Schema 2 was one: it replaced schema 1's `coverage`
  object, whose `run` count meant "a lane the test phase reached listed it",
  with the separate policy and execution objects. A config error before the
  phases run (bad profile name, conflicting flags, a certifies violation)
  emits no summary - resolve-time errors are not run verdicts.

## Invocation selection

Which sweeps each phase attempts, and with which packages, is decided once
before any phase runs, and every phase reads that one answer. Per phase a sweep
is *eligible*, *excluded* (the `-p` set names none of its packages), *deduped*
onto an earlier sweep that compiles the same thing (clippy and rustdoc only -
`check`'s test lanes never dedupe), *not applicable* (rustdoc on a `doc_only`
carrier, and clippy and rustdoc on a doctest carrier a complete claim derived)
or *disabled* (the phase does not run). A sweep that is not eligible builds
nothing for that phase, its `build_packages` included.

A `certifies = "complete"` run first adds the doctest carriers its tested
builds are owed - one `<entry> (doctests)` lane per build no `doc_only` entry
carries, named at the top of the run (see `brokkr man sweeps` under
`parallel`, Doctests).

When `-p` excludes every applicable sweep of an enabled phase, that phase
refuses as a failed run instead of reading green. The test phase's refusal
comes before `prepare`, so a run that will test nothing compiles nothing for it.
The `--json` trailer's `sweeps` lists the sweeps some phase started work on.

The selection model, dedupe keys and the gated phases' selection are in
`reference/check-internals.md`.

## Feature projection

A `[[check]]` entry's `features` are written against the entry's own
selection. An entry over `vm`, `strategy` and `harness` may list
`vm/opcode-counts` and `strategy/hotpath`, and the full run is fine because
some selected package routes every token. A run that selects fewer packages
is a different matter: cargo rejects every token none of its packages routes
("the package does not contain these features"). Three things narrow a run
below the entry's selection:

- **`-p`** on `check`, `brokkr clippy`, or `brokkr test`'s resolved package
  (which always overrides).
- **Package mode** (`feature_unification = "package"`), which runs one cargo
  resolution per package with no `-p` at all.
- **Support builds**: each `build_packages` pre-build selects its one package.

So each request carries the entry's tokens **projected** onto the packages it
selects. For a token and the run's packages P:

- a bare `f` is routed by a package that has a feature `f`, or an optional
  dependency (any kind, any target) named `f` - its implicit feature;
- `x/f` or `x?/f` is routed by a package that declares a dependency (any
  kind, any target) named `x` - the `rename` when there is one, the manifest
  spelling, hyphens kept - or by the package `x` itself when it has `f` by
  the bare rule;
- anything else (`dep:x`, more than one `/`, an empty half) is malformed and
  routes nowhere.

A token some package in P routes is **kept**. A token no package in P routes,
but some member of the entry's **domain** does, is **dropped**. A token nothing
in the domain routes either is **kept**, so cargo rejects it with its own
message, exactly as the full run does; malformed tokens are always kept. The
domain is the entry's own selection for the phase, resolved to workspace
members before any `-p` or per-package split: its `packages`, or every member
minus `test_exclude_packages`, or the workspace's default members. A support
build's domain is that plus every `build_packages` entry. `--all-features`
and `--no-default-features` are flags, not tokens, and pass through
unchanged; when every token is dropped, `--features` is omitted.

A package that depends on the others keeps their qualified tokens: under
`-p harness` (depending on `vm` and `strategy`) the run above carries both
tokens; under `-p strategy` (depending on `vm`) it carries `vm/opcode-counts`
and `strategy/hotpath` but not a `runner/...` token.

Every drop is said once per lane, before any phase runs:

```
sweep piners, -p piners-strategy: dropped piners-runner/trace (applies to piners-runner, piners-harness only)
sweep bin, build piners-runner: dropped piners-strategy/hotpath (applies to piners-strategy only)
```

and the sweep's shape line, a failing command and the parallel lane's repro
line all render the projected features, not the configured list.

**Not projected:** features the invocation names itself - `check --features`
/ `--no-default-features` and `brokkr clippy`'s ad-hoc flags. The user asked
cargo for exactly those, and a token cargo rejects is theirs to see.
`brokkr clippy --sweep NAME` replays an entry, so its features are projected
like any gate sweep's (an identity over the entry's own selection, but a
package-mode entry still projects per package).

The routing reads one `cargo metadata --no-deps` per invocation (each
member's `features` and declared dependencies), fetched without feature
flags, cached, and only when some request selects something other than its
domain while carrying tokens - an ordinary full run pays nothing. Note the
lane's own test runs project against the entry's selection only: a token
that only a `build_packages` package routes is kept there, and cargo rejects
it on the lane's run as it always has. Put support-only features on a sweep
whose selection includes that package.

## Running one textlint rule or script check

`brokkr check --textlint NAME` and `brokkr check --script NAME` run only the
named `[[textlint]]` rules and `[[script_check]]` entries - every other phase
is skipped. Both are repeatable (or comma-separated) and combine. Script checks
run whatever their `stage`, since stages only place them around phases this
run skips.

This is for a person iterating on one failing gate, where a full run would
re-run clippy before reaching it each time. It is never a gate:

- it prints `selected entries only - not a gate` up front;
- clap refuses it beside `--gate`, `--profile`, `-p`, `--features`,
  `--no-default-features`, `--force-rust`, `--json`, `--timings`,
  `--commands`, `--fix-gremlins` and forwarded test args. `--json` is refused
  because its trailer is a verdict a machine could read as a pass;
- a name that matches no entry is an error that lists the known names, so a
  typo cannot print a green run;
- a failing selection ends with exit 1 and no closing `[error]` echo of the
  phases' own reports (they printed in full). An error that is not a phase
  report, such as an io error from the scan, is its only diagnostic and is
  printed.

It selects what runs for a human, never what runs for a gate. That is also why
there is no per-entry "`-p` applies to me" key: that would let a narrowed run
skip a workspace-wide gate.

## Audit

`brokkr check --audit` reports configuration hygiene and runs nothing else.
No phase runs, nothing is built, and no file content is scanned. The
configured selectors are resolved against the tracked file list, and each
finding is printed as a warning:

- a `paths`, `exclude` or `exempt` glob in `[header]`, `[[textlint]]` or
  `[manifest]` (including `shape_exclude`) that selects no tracked file;
- an exclusion that excuses every file its selector picked, and a
  `[manifest]` scope that leaves no manifest;
- a `[gremlins].exclude` entry that matches no scannable file, or that
  removes every one (skipped under `[gremlins].disable = true`).

An ordinary check prints none of this. A user-wide rule legitimately names
paths most trees lack, and an exclusion may reserve a path that is absent
today, so a warning on every run would be noise. The audit is where a real
typo gets found. It ends with `audit: N findings` and exits 0 whatever it
finds. Clap refuses it beside every flag that selects, shapes or certifies
a run (`--textlint`/`--script` included).

## The markdown-only shortcut

In a git repo where **everything uncommitted is markdown**, `check` runs the
`gremlins`, `textlint` and `script_check` phases and nothing else. Clippy and
the tests would be re-proving what the last full run already established on
the same code.

Markdown never counts as code, including markdown a Rust source compiles in
(brokkr's own `include_str!`'d `man` pages, a crate's `#![doc =
include_str!("README.md")]` doctests). This is deliberate, not a bug: a
doctest or rustdoc break from a docs edit surfaces on the next full run, and
`--force-rust` checks the build on demand when a docs edit warrants it. `post-test`
script checks are the exception inside `script_check`: they judge a test phase,
and with none having run they are skipped and counted on a `script-check: N
post-test checks skipped` line, as under a profile that skips `test`.

The classifier is `scope::dirt`, backed by `git::TreeState`
(`git status --porcelain=v1 -z --no-renames --untracked-files=all`). The same
parser supplies unstaged diagnostic priority. Staged, unstaged and
untracked-not-ignored paths all count, `.md` and `.markdown` are prose, and a rename record's *origin* path
counts too - `git mv notes.md src/lib.rs` is not a documentation edit. Four
outcomes, only one of which shortens the run:

| | |
|---|---|
| `Unknown` | not a git repo, or git could not be asked - full run |
| `Clean` | nothing uncommitted - **full run**, since a clean tree is the state a complete check is *for* |
| `ProseOnly` | markdown and nothing else - shortened |
| `Code` | anything else - full run |

**Waived by** `--force-rust`, `--gate`, any `--profile` (and so by any
`certifies` claim), `--features`, `--no-default-features`, `-p`, or trailing
`-- ARGS`. A shortcut that could quietly skip the certifying run would be worse
than no shortcut at all: `--gate` has to mean the same thing on every tree it is
ever run on.

A shortened run says so twice - `markdown-only tree: running gremlins,
textlint, script_check (--force-rust to check the build too)` up front, and
`check passed (markdown only - build phases skipped)` at the end, because the
announcement has scrolled away by the time the verdict is read. The `--json`
trailer carries it as `scope: "prose_only"`.

## `gremlins` phase

Runs first and fails the check if any banned Unicode character
is found in `.rs`/`.toml`/`.md`/`.markdown`/`.js`/`.sh`/`.py`/`.yml`/`.yaml`
files (case-insensitive, tracked or untracked-not-gitignored, so new plan docs
are caught before staging) - see
`src/gremlins.rs` for the banned set (invisible/zero-width, non-breaking
spaces, bidi overrides, em/en dashes, typographic quotes, and emoji /
pictographs: Misc Symbols, Dingbats, the emoji planes, and emoji variation
selectors). The Arrows block (`U+2190..=21FF`) and box-drawing / geometric
shapes (`U+2500..=25FF`) are deliberately spared - both are used legitimately
in comments, formatter output, and tree/table rendering. `--fix-gremlins`
rewrites every banned char in place with its ASCII equivalent (or deletes it
for zero-width/bidi/emoji noise, which have none) before the scan runs, so the
subsequent check finds zero and passes.

`[gremlins]` can exclude directories (vendored material) and adjust the banned
set; see `brokkr man conventions gremlins`.

## `header` phase

Runs next, only when a `[header]` section is present. A file matching
`[header].paths` (minus `exempt`) must contain `[header].pattern` with `{year}`
expanded to the current UTC year. The byte substring may occur anywhere in the
file; the phase does not check its position. Missing text or a stale year
fails. Ported from `check_copyright_year`; see `src/header.rs`.

## `textlint` phase

Runs next, only when `[[textlint]]` rules exist. Each rule forbids a regex on
lines of the files it selects; the keys (`allow_marker`, `except`, `region`,
`skip_after`, `test_code`, `must_scan`, `join_wrapped_use`, ...) are in
`brokkr man conventions textlint`. When a rule with a `test_code` scope selects
any tracked `.rs` file, the phase runs `cargo metadata --no-deps` once to
classify the workspace's test code before scanning.
Beyond those, a rule may carry the
**context-window gates** `except_above` / `except_below` /
`require_above` / `require_below` (each `{ lines = N, pattern = "..." }`).
A gate filters a match by the raw physical lines around it. Each uses the same
behavior: the match is suppressed iff `pattern` is found within `lines` lines
in that direction (excluding the match line, clamped at the file edges); the
names differ only to document intent (`except_above` reads for a
preceding `#[cfg(...)]` exemption, `require_below` for a required token like
`biased;` that must follow a `tokio::select!`). Any matching window suppresses
the violation; it remains only when every configured window is clear. Windows
read raw text - no region masking, no `use`-joining - so because the test is
per-line, write context patterns fragment-tolerant (match `madsim`, not a full
single-line attribute) so a rustfmt-wrapped `#[cfg(...)]` still suppresses.
Reported source excerpts are capped at 200 characters, with an ellipsis when
the matched line is longer; matching still uses the full line.

A clean run reports what it covered - `textlint (21 rules, 812 files)` on the
conventions line - rather than a bare `ok`, matching the `dependency rules`
fragment. The file count
is files at least one rule applied to, not the tracked-file total, so it is a
statement about the corpus that was actually scanned. A rule whose `paths` glob
has stopped matching anything still passes; a shrinking count gives it away,
and a rule that names its sentinel files in `must_scan` fails outright.

## `manifest` phase

Runs next, only when a `[manifest]` section enables a check
(off by default, inert otherwise). It parses each `Cargo.toml` matching
`[manifest].paths` (minus `exclude`) with `toml_edit` and enforces the
configured manifest conventions. When `sort_dependencies` is enabled,
dependency keys must be sorted within each blank-line group;
`[dependencies.<name>]` dotted sections, which TOML forces physically after
the inline table, are their own group and never ordered against it.
`shape_exclude` globs excuse a manifest from the structural checks only
(section/crate-type/package-field order, `[lints] workspace`, bin/example flags
- the same set a `cargo-fuzz = true` stub skips) while still sort-checking it;
`exclude` skips the file entirely. See `src/manifest.rs`.

## `script_check` phase

Runs next, only when `[[script_check]]` entries exist (inert
otherwise). This is the phase's default `pre-clippy` stage; an entry can instead
name `pre-test` or `post-test` and run at that point in the pipeline (see
`stage` below). Each entry runs `command` via `sh -c` (so pipes/redirects/env
expansion work) with cwd = the code tree, and **passes iff the captured output
matches `expect`**. Asserting on a success sentinel - not the exit code - is the
point: it catches a check silently stubbed to `exit 0`, because the script must
prove it ran to completion by emitting the sentinel. The command's exit code is
therefore ignored; only a spawn failure is a hard error. Each entry runs under a
deadline - whatever is left of the `script_check` phase ceiling, less ten
seconds for the report - in its own process group, so a hung script is killed
pipeline and all and fails as that entry (`killed after ...`, with the output it
produced so far) instead of the phase watchdog ending the whole run; a killed
entry never passes, whatever it printed first. Each stream retains at most
1 MiB; if a selected stream exceeds that bound, or a child leaves its output
pipe open past the drain grace, the entry fails because capture cannot prove
the complete stream matched. Every entry runs (no fail-fast within the phase),
so one `brokkr check` surfaces all broken gates, and each failure prints its
retained stdout/stderr - the
diagnostic, rendered against the entry's declared `diagnostics` shape (below).
A clean stage reports once -
`script-check (21 checks)` on the conventions line for the `pre-clippy` stage,
its own `script-check: ok (2 checks)` line for a later stage - rather than one
per entry; the count keeps
the line falsifiable (a stage that quietly stopped running its checks shows a
shrinking number) while a passing gate's name carries nothing to act on. A
partly-failing stage prints `script-check: M of N ok` above the failure block.

The keys (`match`, `stream`, `stage`, `diagnostics`) and what each failure view
prints are in `brokkr man conventions script_check`. For `cargo doc` specifically,
prefer the native [`rustdoc` phase](#rustdoc-phase) over a script check: it
reads cargo's JSON, so each diagnostic is one line with its lint code.

`post-test` entries are skipped when the test phase failed: it fails fast, so
its later lanes never ran and there is no partial-run reading for a sentinel
gate (the coverage audit, which deliberately does run there, reconciles the
plan against the journal, which a failed run has as surely as a green one).
All three stages share the one
`script_check` phase name for `skip_phases` and the JSON `failed_phase`; the
failing entry is named in the output regardless.

## `dependency_rules` phase

Runs next only when `[[dependency_rule]]` entries exist
in `brokkr.toml`; without entries it is skipped silently. It reads
`cargo metadata --no-deps` and fails on configured direct dependency boundary
violations, e.g. `from = "app"` with `forbid = "db"` rejects `app -> db`. A rule
can scope the forbidden match by dependency `kinds` (`normal`/`dev`/`build`,
default all) and `optional` (e.g. `optional = false` to require a dep be
optional), so manifest conventions like "tokio only as a dev-dependency" are
expressible. A rule with `allow` instead of `forbid` inverts the polarity:
every in-scope direct dependency not on the list is a violation, which is how a
leaf boundary stays closed as new crates appear.

## `publish_cycle` phase

The last convention phase, running after `dependency_rules`: it refuses a
dependency cycle among publishable workspace members. `cargo build`
resolves the whole workspace at once and is perfectly happy with one, so
every clippy sweep and test lane stays green; `cargo publish` uploads a
crate at a time and needs each dependency in the published manifest to
already exist on the registry, so a cycle has no valid publication order
and blows up at release time instead.

**Needs no config to arm it.** Unlike its neighbours there is nothing to
declare - a publication cycle is derivable from the manifests alone, is
never intentional, and is invisible to every other phase. It costs one
`cargo metadata --format-version 1 --no-deps`, which is why it can sit in
the always-on tier rather than in a command someone has to remember to
run.

**Ignores the CLI `-p` scope**, deliberately: publication order is a
property of the whole workspace, and a cycle a narrowed run hid would be a
cycle that lands on master. So `brokkr check -p one-crate` still catches
it.

The analysis is `brokkr deps`' `publish_cycle` phase - literally the same
function and the same renderer - so the two commands can never disagree
about a tree. That includes its two exclusions: a version-less
dev-dependency is stripped by cargo on publish and so cannot close a
cycle, while one that names a version does (and gets called out by name,
since deleting the `version` key usually fixes things without
restructuring anything); `publish = false` members are excluded outright.
See `docs/commands/deps.md` for the full rationale.

Prints `publish cycle: ok` when clean - unlike the `deps` section, which
is silent on a clean tree, because a check phase that printed nothing
would be indistinguishable from one that didn't run. On a finding it fails
the check with the loop rendered as a chain:

```
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
cycles sharing a member can surface one at a time (see
`docs/commands/deps.md` for why). Harmless for gating - any cycle fails
the check - but it matters when you are using the output to *scope* a fix,
so re-run after each one rather than trusting a single report as the
complete inventory. The phase points at `brokkr deps` for that, since
scoping is investigation.

Skippable as `publish_cycle` in a partial profile's `skip_phases`.

## `clippy` phase

The clippy phase always invokes cargo with `--message-format=json` and carries
the resulting `Diagnostic` values through ownership, filtering, sorting,
deduplication and rendering. Cargo's pretty-printed text only annotates the
first occurrence of a lint per crate, which is why the JSON path is used.
Cargo's own manifest lints are the one exception - they reach stderr only, as
text, and are parsed into the same `Diagnostic` model (below).

The invocation is `cargo clippy --keep-going --all-targets
--message-format=json <sweep features> -- --cap-lints=warn`. The last two
flags make a single run surface **every** lint across a whole workspace,
instead of the "one error per run" treadmill you get on a large multi-crate
graph:

- `--cap-lints=warn` caps every lint at warn level, so a deny-level lint no
  longer aborts its crate's compile. The crate still produces its `.rmeta`,
  which means every crate *downstream* of a linty one is checked too - the
  whole dependency graph completes in one pass. (Genuine, non-lint compile
  errors are unaffected: they still fail the crate, and `--keep-going` then
  keeps checking the independent branches of the graph rather than stopping
  at the first failure.)
- Because a capped lint lets cargo exit 0, pass/fail is brokkr's own decision,
  not cargo's exit status: **any clippy diagnostic fails the check**, except a
  dependency's warning. A capped `warning` is reported as the error it is, in
  the count and the header. Only messages at `error` or `warning` level are
  diagnostics: rustc's spanless `failure-note` ("For more information about
  this error, try `rustc --explain`") is not. A capped diagnostic keeps
  clippy's `warning:` wording in the underlying JSON; brokkr reports it as the
  error it is, and no view shows the original wording.

The phase's cargo runs - one per build shape, one per package under package
mode - are grouped by the target directory they write (the shared one, or a
sweep's isolated `unify-*`/`rustflags-*` dir). Runs sharing a directory go one
after another in configured order, since cargo would only queue them on its
build-dir lock; the groups run concurrently. Every run is made and the verdict
is decided once all of them are in, exactly as for a serial phase - a lint
found early in one group never cuts another group's runs short. The rustdoc
phase is grouped the same way. `[test] serial_builds`, or a configured cargo
build dir, makes it one group. If Cargo config cannot be fully inspected, the
probe also chooses one group; the Cargo invocation reports unreadable or
invalid config files (`docs/brokkr.toml.sweeps.md`;
`src/check_cmd/concurrency.rs`).

### Cargo's manifest lints

Cargo's own lints (`[lints.cargo]` in `Cargo.toml`: `unused_dependencies`,
`unused_workspace_dependencies`, ...) never reach the JSON stream - cargo
prints them to stderr as text even under `--message-format=json`. The clippy
phase reads them from there (`cargo_json::manifest_lints_by_block`) and lists
them beside clippy's, under the same any-diagnostic-fails rule:

```text
error[cargo::unused_dependencies] crates/t/Cargo.toml:25:1 unused dependency `anyhow`
```

A stderr block is taken for a manifest lint only when its `-->` arrow points
at a `Cargo.toml` *and* it has a lint identity. The arrow alone is not enough:
cargo's ordinary manifest errors (a bad TOML string, an invalid version) carry
the same arrow, and they stay in the captured stream, where their excerpt
explains them. Cargo names the lint (`` `cargo::unused_dependencies` is set to
`warn` ``) only on its first hit per manifest; another hit takes the code of a
named block in the same manifest with the same message shape (its backticked
names emptied), and only when that shape names exactly one lint there. An arrow
with no `line:col` - cargo's fallback when it has no span - still locates the
lint, by manifest alone.

Clippy reads these as findings under its any-diagnostic-fails rule. Test and
support builds also read their stderr manifest lints through the same owner
index so warnings from manifests outside the workspace can be omitted. Rustdoc
does not read them, since clippy already reports the same cargo warning.

Stderr text names no package, so each lint is attributed by the `Cargo.toml`
it names (`attribute_manifest_lints`). Cargo prints that path relative to the
workspace root whatever directory it ran in (a run from a member's parent
directory still prints `crates/a/Cargo.toml`), so it resolves against
`cargo metadata`'s `workspace_root` and is looked up among the members'
manifests:

- **A member's manifest** gets that member's package id and follows the run's
  selection like any other diagnostic: `check -p a` reports `a`'s manifest and
  no other member's.
- **The workspace root's `Cargo.toml`** is reported by every run, whatever it
  selected - it holds the workspace's own tables, which
  `unused_workspace_dependencies` is about.
- **Any other manifest** - cargo runs its manifest-parsing lints over every
  path-sourced package it loads, members or not: a sibling checkout, a
  `[patch]`ed or `exclude`d crate - is a dependency's. Its warning, like any
  dependency's, does not fail the gate; an error fails whoever owns the
  manifest.

Allowing one: the project's own `[lints.cargo]` in `Cargo.toml`, or brokkr's
`[lints] allow` / `allow_exact`
(`cargo::unused_dependencies@crates/t/Cargo.toml`). brokkr applies a
`cargo::` allow where it ingests the diagnostic, never as a `-A` flag - rustc
knows no `cargo` lint tool, so the flag would itself raise a diagnostic - and
leaves `cargo::` names out of the test phase's rustflags for the same reason.

### `[lints] allow`

A `[lints]` section (spelled `[clippy]` historically; the two are unioned)
with `allow = ["clippy::unused_async", ...]` appends
`-A <lint>` after `--cap-lints=warn` on every sweep (and on `brokkr clippy`),
so the listed lints never reach the diagnostic stream - the
any-diagnostic-fails rule needs no carve-outs. This is the escape hatch for
driving a foreign checkout under `disable_toolchain`: brokkr lints on the
host's (newer) clippy, which surfaces lints the project's own pinned-toolchain
CI cannot see and its code cannot be expected to satisfy. The phase announces
the allowed lints up front (`clippy: allowing clippy::unused_async ([lints]
allow)`; under `--commands` and the run log) so a narrowed gate never reads as
a full one, and the `-A` flags ride
in the reprinted failing command. Entries must be bare lint names
(`clippy::`-qualified, `cargo::`-qualified, or plain rustc names); flags are
rejected at parse time. A `cargo::` entry is the exception to the `-A` route:
it acts at ingestion (see Cargo's manifest lints, above).

**Known limit of `allow`:** the injected `-A` flags act at the CLI lint
level, and a source site that carries its own lint-level attribute can
override them. The observed shape (clippy 1.98, nautilus_trader
a930c8afe3): a function with `#[expect(clippy::unused_async, ...)]` still
fired the sibling lint `clippy::unused_async_trait_impl` **at error
severity** with both `-A clippy::unused_async_trait_impl` and
`--cap-lints=warn` on the command line - the expectation machinery's
diagnostic bypassed both. The same lint was suppressed fine at
attribute-free sites. Best reading: a clippy expectation-machinery
interaction, not a brokkr defect - but the practical consequence is that
`[lints] allow` cannot be relied on to silence a lint at a site holding an
`#[expect]` for a sibling lint emitted by the same pass. Minimal upstream
repro sketch: an inherent `async fn` with a tail expression and no
`.await`, `#[expect(clippy::unused_async)]`, compiled with
`-A clippy::unused_async_trait_impl` on clippy 1.98.

### `[lints] allow_exact`

`[lints] allow_exact` is the remedy for exactly that shape: `"lint@path"`
entries suppressed on **brokkr's side of the pipe** instead of the
compiler's. A matching diagnostic - same lint (with or without the
`clippy::` qualifier), same build-root-relative file - is dropped at JSON
ingestion, after clippy has spoken and before the any-diagnostic-fails
decision, so no lint-level attribute at the site can defeat it. It is
deliberately narrow where `allow` is broad: one lint in one file (every
occurrence in that file - file-granular by design, since line numbers drift
with unrelated edits), never workspace-wide, and no `-A` is injected for it,
so other sites of the same lint keep failing the check. The entries' distinct
lints are named on the verdict line (`allow_exact: clippy::assert_is_empty,
deprecated`), and the scope is this paragraph's, not the line's: an entry is
sited per file in the diagnostic phases (clippy and rustdoc), but the
compiling phases (test, coverage enumeration, member-check) have no
per-file mechanism, so there it applies build-wide through rustflags. `cargo::`
entries never reach a build and are marked `(sited)` on the verdict. The
per-lint file-count summary (`clippy: allowing
clippy::assert_is_empty (59 files), deprecated (3 files) ([lints]
allow_exact)`) goes to the run log, and prints under `brokkr clippy` or
`--commands`. (The blanket `clippy: allowing ... ([lints] allow)` line is
log-only under `brokkr clippy`, whose printed command already carries the `-A`
flags.) Naming the lints is what keeps a narrowed gate from reading as a
full one; the paths are already in `brokkr.toml`, and one line per entry
buries the rest of the run once a project has more than a handful. An entry
that suppressed nothing across the run draws a
`suppressed nothing (stale entry?)` warning - upstream fixed the site or the
file moved, so the entry should be deleted or re-sited rather than accrete.
The notice only fires on unscoped runs: a `-p`-narrowed run doesn't check an
entry's file when it lives outside the selected packages, so "suppressed
nothing" there proves nothing. `brokkr clippy` applies the same rule to its
one probe shape, and stays silent unless that shape is unscoped, lints every
target (no `--lib`) and uses `--all-features`. Each phase judges only the
entries it can produce: clippy the non-`rustdoc::` ones, the rustdoc phase the
`rustdoc::` ones (so a project without `[rustdoc]` never judges those).
A suppressed diagnostic is dropped from both the pass/fail decision and the
output; nothing shows it. The path half must match the file exactly as clippy reports it
(relative to the tree cargo compiles in - copy it from the failing
diagnostic's location).

## `rustdoc` phase

Runs after clippy and before the `pre-test` script checks, only when
`brokkr.toml` has a `[rustdoc]` table (an empty one is enough). Rustdoc is the
one compiler pass neither clippy nor the tests run, so without this phase a
project's `rustdoc::` lints are declared and never evaluated, and a doc link
broken by a file split rots unseen.

The invocation is `cargo doc --no-deps --keep-going --message-format=json`,
plus `--document-private-items` when `[rustdoc] document_private_items = true`,
plus the sweep's selection exactly as clippy builds it: profile, feature
unification, packages, features, env and isolated target dir. Rustdoc resolves
`cfg` like the build does, so documenting under any other shape would judge doc
comments on code that shape never compiles. It runs once per distinct build
shape, deduped and `-p`-intersected by the same rules as clippy (see
"Invocation selection") and through the same loop (`run_per_build_shape`), so
the two phases cannot disagree about what a sweep covers. A `doc_only` sweep
is not applicable (`skipped (doctest carrier, no build shape of its own)`):
it exists to run doctests, so documenting it re-reports
another sweep's diagnostics under a second label - and cannot always be
deduped, since a carrier that inherits feature unification from
`.cargo/config.toml` differs on argv from a sibling that passes it.

On a nightly rustdoc that supports it, the phase lints without rendering
(`-Zunstable-options --check`), detected automatically; there is no key either
way. The reported lints are the same. How the flags are placed and how doc
freshness is kept are in `reference/check-internals.md`.

Output goes through clippy's formatter: one `error[<lint>] <file>:<line>:<col>
<message>` line per diagnostic, merged across sweeps, nothing capped. A
diagnostic is tagged with the sweeps that reported it only when some sweep
covering its package did not (a link that breaks only with a feature off).

**Any diagnostic fails the phase, warnings included**, as for clippy: rustdoc's
lints are warn-by-default, so a gate failing only on `error` would pass every
broken link. Suppression happens at ingestion:

- `[lints] allow` drops a diagnostic whose lint code equals an entry exactly
  (`rustdoc::private_intra_doc_links`; group names match nothing). Prefer
  `Cargo.toml`'s `[lints.rustdoc]` for a rustdoc lint: brokkr's list also
  reaches the test phase's rustflags, and a new entry costs a full test rebuild.
- `[lints] allow_exact` drops `lint@path` sites. This phase judges the
  `rustdoc::` entries for staleness, clippy judges the rest.

Skippable as `rustdoc` in a partial profile's `skip_phases`, skipped on a
markdown-only tree, with a 5-minute ceiling like clippy.

## `certifies` and exit codes

A profile may declare `certifies = "complete"` or `"partial"` (see
`docs/brokkr.toml.sweeps.md`); the claim decides the success word, the exit code,
and which narrowing is permitted:

| profile | success line | exit on success | exit on failure |
|---|---|---|---|
| no `certifies` (legacy) | `check passed` | 0 | 1 |
| `certifies = "complete"` | `check complete` | 0 | 1 |
| `certifies = "partial"` | `check partial (...)` | **10** | 1 |

Partial's exit 10 is the point: `brokkr check && git commit` on a partial
profile fails closed, so a loop answer cannot silently substitute for a gate
answer. The partial success line lists what was narrowed (skipped phases,
`-p` scope) and never contains the word `passed`. `skip_phases` on a partial
profile skips the named phases and announces them up front; under a
`complete` profile, `-p` is rejected before anything compiles (a scoped
build's green is not comparable to the full build's - feature unification
changes with the package set). Trailing `-- ...` test args are rejected the
same way under `complete`: a libtest `--skip` or a cargo `--lib` narrows
the real run but not the plan, which is built from the sweeps' own filters.
2 = clap usage errors, 124 = a time ceiling fired, 130 = interrupt.

## Per-sweep log lines (run log and failures only)

Each sweep describes itself as `<phase> <name>: <shape>` rather than its full
cargo command. A passing run never prints these: the shape is a function of
the config and the invocation, identical run to run, so on green it carries
no news - what this invocation changed prints on the `invocation:` line
instead (see "Output"). The shape goes to the run log with the full argv
under it, and to the status line on a terminal:

```
[run]     profile tier1: 3 sweeps (default, ffi, live)
[run]     clippy default: default selection
[run]     cargo clippy --keep-going --all-targets ...
[run]     clippy ffi: 4 pkgs, +ffi
[run]     test default: workspace -2 pkgs, 14 skips, parallel
```

The package part of a shape is the phase's selection for that sweep (see
"Invocation selection"): the default sweep's clippy line above passes no
package flag, so it reads `default selection`, while its test line carries the
`test_exclude_packages` exclusion. A failing sweep prints its shape beside its failing command. Resolved facts
the config does not spell out - `feature_unification = "auto"` promoting a
lane, the topology-derived parallel budget - are machine- and tree-stable and
stay log-only; they are the data for a cross-host comparison, not for
reading a verdict.

The full command is ~90% profile boilerplate repeated identically per sweep -
on nautilus_trader the three `cargo test` lines are ~1,100 chars each, of which
~900 are the same 14 `--skip` flags, because those come from the *profile*, not
the sweep. What actually varies is package scope and features, which is what
the shape carries.

The shape is `<package scope>[, <features>][, rustflags ...][, <test bits>]`:

- package scope - `workspace`, `N pkgs` (a `packages` list, emitted as `-p`),
  or `workspace -N pkgs` (`test_exclude_packages`; test phase only, since
  clippy stays workspace-wide).
- features - read back out of the request's feature argv (the projection, not
  the configured list - see [Feature projection](#feature-projection)), so it
  cannot drift from what cargo is handed: `all-features`, `no-default`,
  `+ffi,live`. A lane-level line shows the tokens some run of the lane
  carries. A fragment that
  merely restates the sweep's name is dropped (the legacy no-`[[check]]` path
  names its synthesized sweep `all-features`).
- `rustflags <flags> (isolated target)` - always part of the shape, because
  `rustflags` silently redirects the sweep to `target/rustflags-<hash>`, and
  the log is where an unexplained full recompile gets explained.
- test-phase bits - `N skips`, `include-ignored`, any `--test <name>` filters,
  and the lane (`serial` under the per-test watchdog, `parallel` otherwise).

**Failures always print the shape and the full command**, as `[error] <phase>
<name>: <shape>` and `[error] failing command: cargo ...` - when a sweep fails,
the copy-pasteable line is the most useful thing in the output. This covers
clippy and rustdoc failures, test failures, hung tests, parallel-sweep
timeouts, zero-test runs, `build_packages` pre-build failures, and any error
leaving a test lane, the nextest lane's `cargo metadata` call included. A
failure that has printed its full diagnosis ends in a `Reported` label (a
blown time budget, a failed pre-build, a lane that could not be prepared): the
label is suppressed by check's summary, so every fact, including the package,
sweep or test identity, must appear in the printed block itself. `--commands`
does not suppress any of this: the line that flag
streamed earlier is neither beside the failure nor attributable among several
runs.

`--commands` prints each cargo command on stdout before it runs - every
sweep's, the pre-builds', the enumeration's, member-check's and its probes',
fixture-lint's, and the
`cargo metadata` invocations of the dependency-rule and publish-cycle phases.
Streamed, not collected, so a run the watchdog kills still shows what it was
running. The grouped green lines are unchanged by it. `brokkr clippy` is
unaffected by the flag and always prints its command, once and after the run:
it is the investigative runner, invoked precisely to find out what a given
target shape does. A green run prints the cargo line; a failing one prints the
shape and `failing command:` pair above (also on spawn failures, interrupts
and deadlines), and does not stream the line first as
well. Its ad-hoc sweep is labelled `clippy`, and the pair reads `[error]
clippy: <shape>` rather than `clippy clippy:`.

Member-check's green line carries only the run's counts and profile
(`member-check: ok (3 members, 3 libs, 2 bins, dev) in 8.1s`, or `1 of 3
members, ...` under a `-p` that narrowed the set); the constant explanation of
the phase (each member resolved on its own like `cargo install`, shared
lockfile, no codegen) is log-only. Fixture-lint's names each fixture's
variants, files, expectations and target-dir size. The shim-fallback warning (`harness isolation unavailable (...)`)
prints once per sweep per run, not once per cargo resolution of a package-mode
sweep.

## Sweep selection

| invocation | sweep set | libtest filters |
|---|---|---|
| no `[[check]]`, no flags | one `--all-features` sweep (legacy default) | none |
| `[[check]]` configured, no `default_profile`, no flags | every `[[check]]` entry in declaration order | none |
| `[[check]]` + `default_profile = "tier1"`, no flags | the entries `tier1.sweeps` references | tier1's filters |
| `--profile tier1` | the entries `tier1.sweeps` references | tier1's filters |
| `--features X` (or `--no-default-features`) | one ad-hoc sweep, no `build_packages` | the resolved profile's filters (see below) |

**CLI features override sweep *selection*, not run shaping.** The two are
independent: sweep selection decides what cargo compiles, while a profile's
`skip` / `only` / `tests` / `lib_tests` / `exclude_tests` / `include_ignored` /
`test_threads` / `isolation` / `env` decide which tests run and how. A test that cannot pass in-process
cannot pass in-process at any feature shape, so an ad-hoc run inherits the
shaping of the profile it would otherwise have used (`--profile NAME`, else
`[test] default_profile`).

It also inherits **entry-level `env`, unioned across every `[[check]]`
entry** - the same `merge_check_envs` `brokkr clippy`'s ad-hoc path uses, so
the two commands resolve one shape from one config. An entry `env` is treated
as a build-affecting invariant - a var a build script reads, a codegen toggle
- and a probe that drops one can go green having compiled something other
than what the gate compiles. A key two entries set to *different* values is a
hard config error naming the key, never a coin flip. Entry env overlays
profile env on a collision, matching non-ad-hoc sweeps.

brokkr cannot tell a load-bearing entry `env` from an inert one - that would
mean knowing what every `build.rs` reads - so it carries all of them. Note
what this does **not** cover: where a project's real invariant is a *cargo
feature* rather than an env var, the shape follows feature resolution and
therefore follows the CLI scope, which no env union can restore. An entry
`env` that nothing reads is harmless but proves nothing; do not read its
presence as evidence that a scoped run matched the gate's shape.

It still takes no `[[check]]` entry, so it inherits no entry-level
`features`, `test_exclude_packages` or `build_packages` - see the warning
below.

A `lanes` profile shapes nothing of its own (lanes carry no run-shaping
fields), so an ad-hoc run under one inherits no filters rather than silently
borrowing one lane's.

Every ad-hoc run names what it inherited, `--commands` included (that flag adds
the cargo lines and suppresses nothing):

```
[run]     ad-hoc features: sweep selection overridden, run shaping from profile tier1
[run]     ad-hoc features: sweep selection overridden, no profile - no test filters applied
```

An ad-hoc run reports no profile in the header or the `--json` trailer, because
it claims no `certifies` - this line is the only statement of its filters, and
without it a red is indistinguishable from a code failure.

> [!WARNING]
> An ad-hoc run takes no `[[check]]` entry, so **`test_exclude_packages` is
> not applied** - an entry excluding a package from its test selection (say,
> one that would link libpython) will try to link it here. `env` *is*
> inherited, via the union above; exclusions are deliberately not, because
> they are a per-entry selection workaround rather than an invariant, and
> unioning them would narrow what a scoped run tests while the rest of the run
> reads wider. Prefer a profile when the entry's selection matters.
>
> A shape needing an entry's exclusions *and* different features
> simultaneously is not expressible as any union, and has no spelling today.

A profile with `lanes` resolves to the concatenation of its lanes' sweeps,
labels lane-qualified (`tier1/default`, `serial/default`). The test phase
runs each lane's entry separately - contradictory filter sets are the point -
while the clippy and rustdoc phases dedupe sweeps that compile the same thing
(the same requests - selections, projected features, support builds - and the
same rustflags, env, profile and effective unification; see "Invocation
selection"), logging `clippy <label>: deduped`. `profile` is in the key because
`cfg(debug_assertions)` decides which code exists: a dev and a release sweep of
the same features present different lint surfaces, so they are linted
separately rather than deduped into one.

`brokkr test <name>` follows the same ladder except: filters are dropped (the
user's `<name>` argument is the filter), there's no CLI ad-hoc path (the
test runner doesn't accept `--features`), and sweeps that would execute
identically for the resolved package fold into the first (with filters
dropped, lane duplicates are identical runs) - but a sweep and its `doc_only`
twin never do, nor two sweeps with different `build_packages` (see
"Invocation selection"). `--sweep` applies before that fold, and its labels
under a lanes profile are the lane-qualified form.

Per-project orchestration blocks (today: `[ratatoskr.harness]`) are **not**
`[[check]]` sweeps and are invisible to both `brokkr check` and `brokkr test`.
They describe how to build a binary that ratatoskr's orchestration commands
(`service`, `mock-serve`, `sync`)
spawn, with their own `package` / `features` / `debug` fields. `[test.profiles]`
may only reference `[[check]]` entries in its `sweeps` list, never an
orchestration block.

