# brokkr check + brokkr test

Both commands share the sweep + profile machinery in `src/profile.rs` and the
test-phase logic in `src/check_cmd.rs`. They differ in scope: `check` is the
full validation pass (gremlins + header + textlint + manifest + script checks +
dependency rules + publish cycle + clippy + tests); `test` runs one named cargo
test against the same sweep set.

For the underlying config (`[[check]]`, `[[dependency_rule]]`, `[test]`
section, profiles) see `docs/brokkr.toml.md`.

Sections below follow the pipeline: the command itself, then one section per
phase in the order they run, then the machinery layered on top (coverage,
`certifies`, log lines, sweep selection), then `brokkr test`.

## `brokkr check`

Gremlins + header + textlint + manifest + script checks + dependency rules +
publish cycle + clippy + tests. Trailing args after
`brokkr check --` are split on a literal `--`: tokens before it go to
`cargo test` (e.g.
`brokkr check -- --test cli_sort` scopes to one test crate), tokens after go
to libtest after the default `--test-threads=1` (e.g.
`brokkr check -- -- --ignored`). With no second separator, every token is
cargo-level. The leading `--` is **required**: `check` takes no
`trailing_var_arg`, so an unrecognised brokkr flag is a parse error rather
than a token silently forwarded to `cargo test`. The test phase also fails on a successful `cargo test` that ran
zero tests (suites=0, or filters excluded everything) so a too-narrow
profile/filter combo can't silently green-light a check. Each test sweep
closes with a `[test]    N passed` count line (`, M ignored` / `, K filtered
out` appended when non-zero) - the symmetric bookend to `running tests`, so a
green run always says how much it ran. A suite that *legitimately* ran zero
tests (nothing filtered out - e.g. an all-doctest crate, since `--tests`
excludes doctests) still passes; on an explicit `-p <pkg>` spot-check that ran
nothing, an extra warning notes the green validated clippy, not tests. The
warning is scoped to the hand-typed `-p` path so a whole-workspace run never
nags.

Like every locked brokkr command, `check` and `test` acquire the global
per-user lock **blocking**: if another brokkr invocation (e.g. a bench run)
holds it, the command prints `[lock] waiting for …` and waits until released,
then proceeds - rather than failing with `lock: already locked`. So a
concurrent lock never produces an error to handle; just let the command wait.

Flags:
- `-p/--package <PKG>` (repeatable) - scope every sweep's cargo invocation
  (clippy + test) to the named packages. The set **replaces** each sweep's
  own package selection - cargo unions selection flags, so composing
  `--workspace --exclude …` with `--package` would silently un-scope the
  run. Per sweep the set is *intersected* with the sweep's scope: a package
  its `packages` list or (test phase only) `test_exclude_packages` rules out
  is dropped with a log line, a sweep keeping none is skipped (mirroring
  `brokkr test`'s SKIP) - so `-p a -p b` still reaches `a` in the sweep that
  admits it when `b` lives in another sweep. If every sweep skips, the phase
  fails rather than reading as green. The shape line shows `-p <pkg> …` and
  the `--json` summary carries a `package` field (comma-joined for a
  multi-package run). Rejected under a `certifies = "complete"` profile
- `--features` / `--no-default-features` - ad-hoc sweep, no `build_packages`.
  Overrides sweep *selection* only; the resolved profile's run shaping (skips,
  filters, thread policy) still applies - see "Sweep selection"
- `--profile <NAME>` - selects a `[test.profiles]` entry; conflicts with
  `--features` / `--no-default-features`
- `--gate` - run the profile named by `[test] gate_profile` (load-validated
  to certify "complete"). The stable pre-commit invocation. Conflicts with
  `--profile`, `--features`, `--no-default-features`, and `-p`. Trailing
  `-- …` test args are rejected under any `complete` claim (see `certifies`)
- `--raw` - unfiltered cargo output (terminal-style rendering)
- `--json` - append one machine-readable summary line (a JSON object) as the
  last line of stdout; human output is unchanged
- `--limit N` - max diagnostics shown per phase (gremlins, clippy, and the
  `--timings` list), default 20
- `--triage` - show every gremlins/clippy diagnostic and every `--timings` row, no
  cap, no changed-files scoping. Does *not* widen the test phase - the failure
  list is never capped or scoped in the first place
- `--fix-gremlins` - rewrite banned chars in place before scan
- `--commands` - log each sweep's full cargo command instead of the collapsed
  form (see the log-lines section)

Output:
- Default text mode: each diagnostic becomes one line, compilation noise
  stripped, passing tests aggregated.
- `--raw` reconstructs cargo's terminal-style output by concatenating each
  diagnostic's `rendered` field plus the cargo status messages on stderr -
  one cargo invocation.
- When hits exceed `--limit`, both the gremlin and clippy phases prefer files
  changed on the current branch (computed via git merge-base against
  `@{upstream}` / `origin/master` / `origin/main`) and append a trailer
  summarising what's hidden; see `src/scope.rs`.
- **The cap never hides an error.** It is a warning-volume control, so clippy
  *errors* are pinned through `scope::partition_pinned`: they display in full
  however far past `--limit` they sort, and they do not consume cap slots that
  would otherwise show warnings. An elided error reads as "not in this run" to
  anyone who trusts the list, and the trailer only counts what it hid without
  saying that one of them was fatal.
- `--json` appends one summary object as the **last line of stdout**, leaving
  the human output untouched (the old NDJSON per-event mode is gone; this is
  the result contract). Fields: `schema` (currently
  1), `certifies` (the resolved profile's claim, `null` for unclaimed
  profiles), `verdict` (`"passed"`/`"complete"`/`"partial"`/`"failed"`),
  `profile` (the profile that drove sweep selection; `null` for ad-hoc and
  legacy runs), `sweeps` (labels), `package` (the CLI `-p` scope, `null`
  when the run was not scoped; multiple `-p` packages comma-joined), `failed_phase` (`null` on success, else one
  of `gremlins`/`header`/`textlint`/`manifest`/`script_check`/
  `dependency_rules`/`publish_cycle`/`clippy`/`test`/`coverage`), `elapsed_ms`. The object is versioned
  and additive: fields are only ever added under `schema: 1`, consumers must
  tolerate unknown fields, and a bump is reserved for renames or semantic
  changes. A config error before the phases run (bad profile name,
  conflicting flags, a certifies violation) emits no summary - resolve-time
  errors are not run verdicts.

## The markdown-only shortcut

In a git repo where **everything uncommitted is markdown**, `check` runs the
`gremlins`, `textlint` and `script_check` phases and nothing else. Documentation
cannot change how the code builds, so clippy and the tests would be re-proving
what the last full run already established on the same code.

The classifier is `scope::dirt` (`git status --porcelain=v1 -z
--untracked-files=all`): staged, unstaged and untracked-not-ignored paths all
count, `.md` and `.markdown` are prose, and a rename record's *origin* path
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
announcement has scrolled away by the time the verdict is read.

## `gremlins` phase

Runs first and fails the check if any banned Unicode character
is found in `.rs`/`.toml`/`.md`/`.js`/`.sh` files (tracked or
untracked-not-gitignored, so new plan docs are caught before staging) - see
`src/gremlins.rs` for the banned set (invisible/zero-width, non-breaking
spaces, bidi overrides, em/en dashes, typographic quotes, and emoji /
pictographs: Misc Symbols, Dingbats, the emoji planes, and emoji variation
selectors). The Arrows block (`→` and friends) and box-drawing / geometric
shapes (`U+2500..=25FF`) are deliberately spared - both are used legitimately
in comments, formatter output, and tree/table rendering. `--fix-gremlins`
rewrites every banned char in place with its ASCII equivalent (or deletes it
for zero-width/bidi/emoji noise, which have none) before the scan runs, so the
subsequent check finds zero and passes.

A `[gremlins]` section with `exclude = ["docs/manual", ...]` skips listed
directories in both the scan and `--fix-gremlins`. Use it for vendored
material from an outside source (reference manuals, imported docs) that
legitimately carries typographic punctuation, BOMs, and the like. Matching is
by path prefix on the git-relative path, so `docs/manual` covers
`docs/manual/` and everything beneath it but not a sibling `docs/manual-extra`.
Empty and absolute entries are rejected at parse time.

## `header` phase

Runs next, only when a `[header]` section is present. A file
matching `[header].paths` (minus `exempt`) must contain `[header].pattern` with
`{year}` expanded to the current UTC year; a missing header or a stale year
fails. Ported from `check_copyright_year`; see `src/header.rs`.

## `textlint` phase

Runs next, only when `[[textlint]]` rules exist. Each rule
forbids a linear-time regex `pattern` on lines of files matching `paths` (minus
`exclude` globs); a match is a violation, subject to bounded modifiers:
`allow_marker` (+ `allow_marker_above = N` for a marker up to N lines above),
`except`, `in_toml_section`, `table_row_only`, `skip_after` (a regex past which
the rest of a file is exempt, e.g. to ignore a test module),
`only_if_file_matches` (a file-scope precondition regex; add
`only_if_file_matches_above = true` to require the precondition at or above each
match rather than anywhere in the file, so an import below the match - e.g.
inside a test module - no longer arms the rule), `region`
(`code`/`string`/`comment` - scope the pattern to a lexical region of a Rust
file, tokenized with `rustc_lexer`, so a rule never fires on a match quoted in
a comment or string), `join_wrapped_use` (match against whole `use ...;`
statements, reconstructing a rustfmt-wrapped import onto one line first), and
the four **context-window gates** `except_above` / `except_below` /
`require_above` / `require_below` (each `{ lines = N, pattern = "..." }`).
A gate filters a match by the raw physical lines around it: all four have the
same behavior - the match is suppressed iff `pattern` is found within `lines`
lines in that direction (excluding the match line, clamped at the file edges) -
and the names differ only to document intent (`except_above` reads for a
preceding `#[cfg(...)]` exemption, `require_below` for a required token like
`biased;` that must follow a `tokio::select!`). Multiple gates AND together
(the violation stands only when every window is clear). Windows read raw text -
no region masking, no `use`-joining - so because the test is per-line, write
context patterns fragment-tolerant (match `madsim`, not a full single-line
attribute) so a rustfmt-wrapped `#[cfg(...)]` still suppresses. The generic
engine behind most grep-style convention hooks; see `src/textlint.rs`.

A clean run reports what it covered - `textlint: ok (21 rule(s), 812 file(s))`
- rather than a bare `ok`, matching the `dependency rules` line. The file count
is files at least one rule applied to, not the tracked-file total, so it is a
statement about the corpus that was actually scanned: a rule whose `paths` glob
has stopped matching anything still passes, and a shrinking count is the only
thing that gives it away.

## `manifest` phase

Runs next, only when a `[manifest]` section enables a check
(off by default, inert otherwise). It parses each `Cargo.toml` matching
`[manifest].paths` (minus `exclude`) with `toml_edit` and enforces structural
conventions - today `sort_dependencies` (dependency keys sorted within each
blank-line group; `[dependencies.<name>]` dotted sections, which TOML forces
physically after the inline table, are their own group and never ordered against
it). `shape_exclude` globs excuse a manifest from the structural checks only
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
therefore ignored; only a spawn failure is a hard error. Every entry runs (no
fail-fast within the phase) so one `brokkr check` surfaces all broken gates, and
each failure prints the full captured stdout/stderr (the diagnostic, never
truncated by `--limit`). A clean stage prints a single collapsed line -
`script-check: ok (21 check(s))` - rather than one per entry; the count keeps
the line falsifiable (a stage that quietly stopped running its checks shows a
shrinking number) while a passing gate's name carries nothing to act on. A
partly-failing stage prints `script-check: M of N ok` above the failure block. It fills the gap for gates brokkr's native phases can't
express - semantic analysers (`# Panics`/`# Errors` doc checks) or external
formatter conventions - that were previously hand-run before every commit.

- `match` = `exact` (whole trimmed stream equals `expect`; suits quiet lints
  that print only the sentinel), `last-line` (the last non-empty line, trimmed,
  equals `expect` - the **default**; tolerates progress output above a final
  verdict), or `contains` (`expect` is a substring).
- `stream` = `stdout` (default), `stderr`, or `both` (stdout, a newline, then
  stderr - for tools that split progress and results across the two).
- Sentinel tip: a non-ASCII sentinel (e.g. a `U+2713` check mark) would itself
  trip the gremlin scan on `brokkr.toml`. Use an ASCII sentinel, or
  `match = "contains"` on an ASCII marker substring of the real success line.
- `stage` = `pre-clippy` (default - here, with the other convention phases),
  `pre-test` (after clippy, before the test phase), or `post-test` (after the
  test phase and the coverage audit). One value per entry; an entry runs once.

`post-test` entries are skipped when the test phase failed: it fails fast, so
its later lanes never ran and there is no partial-run reading for a sentinel
gate (the coverage audit, which deliberately does run there, wants built
binaries rather than green tests). All three stages share the one
`script_check` phase name for `skip_phases` and the JSON `failed_phase`; the
failing entry is named in the output regardless.

See `src/script_check.rs`.

## `dependency_rules` phase

Runs next only when `[[dependency_rule]]` entries exist
in `brokkr.toml`; without entries it is skipped silently. It reads
`cargo metadata --no-deps` and fails on configured direct dependency boundary
violations, e.g. `from = "app"` with `forbid = "db"` rejects `app -> db`. A rule
can scope the forbidden match by dependency `kinds` (`normal`/`dev`/`build`,
default all) and `optional` (e.g. `optional = false` to require a dep be
optional), so manifest conventions like "tokio only as a dev-dependency" are
expressible.

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
[error]   publish cycle: 1 cargo publication cycle(s)
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

Honours `--limit`/`--triage` like the other finding phases, and is skippable
as `publish_cycle` in a partial profile's `skip_phases`.

## `clippy` phase

The clippy phase always invokes cargo with `--message-format=json` and ingests
via `cargo_json::parse_cargo_diagnostics` regardless of `--raw` - the text
formatter converts each `DiagnosticEvent` into a `ClippyDiagnostic` so every
warning keeps its lint code in the header, even for repeats of the same rule
(cargo's pretty-printed text only annotates the first occurrence per crate,
which is why the JSON ingestion path was needed; see `src/cargo_filter.rs`
module header).

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
  not cargo's exit status: **any clippy diagnostic fails the check.** brokkr
  treats a capped `warning` as the deny it really is - `event_to_clippy`
  promotes it back to `error` for counting and the header, so the output never
  misleads with "0 errors, N warnings" while failing. The `--raw` escape hatch
  still dumps clippy's own rendered text verbatim (which shows the capped
  `warning:` wording).

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
allow)`) so a narrowed gate never reads as a full one, and the `-A` flags ride
in the reprinted failing command. Entries must be bare lint names
(`clippy::`-qualified or plain rustc names); flags are rejected at parse time.

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
so other sites of the same lint keep failing the check. Each entry is
announced up front, but *collapsed*: entries are grouped by lint with a file
count (`clippy: allowing clippy::assert_is_empty (59 files), deprecated (3
files) ([lints] allow_exact)`), and a lint with a single site keeps its path
instead. Naming the lints is what keeps a narrowed gate from reading as a full
one; the paths are already in `brokkr.toml`, and one line per entry buries the
rest of the run once a project has more than a handful. An entry that
suppressed nothing across the run still draws a
`suppressed nothing (stale entry?)` notice - upstream fixed the site or the
file moved, so the entry should be deleted or re-sited rather than accrete.
The notice only fires on unscoped runs: a `-p`-narrowed run doesn't check an
entry's file when it lives outside the selected packages, so "suppressed
nothing" there proves nothing.
`--raw` still shows suppressed diagnostics (it dumps clippy's own rendered
text verbatim); the pass/fail decision and the formatted output do not
count them. The path half must match the file exactly as clippy reports it
(relative to the tree cargo compiles in - copy it from the failing
diagnostic's location).

## `test` phase

Runs after clippy, one lane per selected sweep. The subsections below cover
the failure list, build-error reporting, lint suppression, per-sweep
`rustflags`, the serial and parallel lanes, process isolation, and doctest
policy.

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
- **The flags travel through cargo's rustflags,** since `cargo test` has no
  `-- <rustc flags>` passthrough. Cargo picks exactly one rustflags source, so
  brokkr finds the layer already live for this build and adds to *that* one
  with `--config` (the highest-precedence config source) rather than exporting
  `RUSTFLAGS`, which would discard the project's own flags wholesale. The
  notice line names the layer it used. See
  [`[lints]`](../brokkr.toml.md#lints-section) for the full rule, including why
  an injection can be deliberately inert.

The flags reach **every call site in the run that compiles**, not only the
`cargo test` invocation: the sweep pre-build (which would otherwise fail on the
unsuppressed lint before `cargo test` was ever reached), the process-isolated
lane's enumeration and per-test invocations, and the coverage audit's
`cargo test --no-run` enumeration.

That last one is worth stating because it is where the rule was learned. The
audit runs last, so a call site missing the injection turns a run whose every
lane went green into a failure with no verdict - from the caller's side
indistinguishable from a real one until you read which phase the error came
from. The test that guards it asserts the property rather than the call site:
enumeration is assembled from the same selection the allows are prepended to.

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
understood. Note that `--triage` does not widen test reporting either - it
governs clippy and gremlins capping and scoping.

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
- **The `failures:` name list is the authoritative roster.** The captured
  detail blocks are best-effort - an aborted suite can truncate the stream,
  and a detail block can hide inside another test's captured output. Any name
  the roster lists that the detail pass missed is appended bare (name, no
  location) rather than dropped.
- **libtest's `test result:` tally outranks the parsed roster for the
  headline count.** `cargo test: N failures` reports
  `max(tally, roster.len())`, and when the tally is larger the report says how
  many failures could not be attributed to a test name and points at `--raw`.
  Relatedly, an empty roster no longer routes to the compact "all passed"
  summary when the tally is non-zero.

Pinned by `the_name_list_completes_a_short_failure_roster`,
`the_rendered_report_names_every_failing_test` and
`an_unattributable_failure_is_counted_not_dropped`.

Unrelated but worth knowing beside it: a failing sweep ends the run before the
later `[[check]]` sweeps test at all, so their results are simply absent from
a red run. That follows from phase ordering, not from any reporting limit.

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
also relabels the summary line `cargo test:` - the diagnostics ride through
`filter_clippy`, but the command that produced them was `cargo test`).

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

### Foreign manifest warnings

A *passing* test sweep still renders whatever cargo wrote to stderr, condensed
through `filter_clippy` and relabelled `cargo test:`. Cargo emits its own
manifest complaints there - deprecated `lints.*` keys, unused manifest keys -
once per dependency manifest it loads, path dependencies included. A project
that builds against a vendored fork therefore collected dozens of lines about a
`Cargo.toml` it does not own and cannot edit, on every run.

The test phase calls `filter_clippy_in_tree` with the project root instead, and
warnings whose message heads with an absolute `Cargo.toml` path outside that
root are dropped, not merely deprioritised - they are not diagnostics about
this repo. The shape is specific (no span, absolute path, filename
`Cargo.toml`), so ordinary lints are untouched, and warnings about the
project's own manifests still print. The clippy phase never needed this: it
ingests `--message-format=json` compiler messages, which exclude cargo's own
stderr.

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
example in `docs/brokkr.toml.md`.

### Serial vs parallel sweeps

By default the test phase runs each sweep serial (`--test-threads=1`) under
the per-test hang watchdog, attributing a stall to a named test
from libtest's sequential output. The parallel lane keeps the same watchdog via
libtest's JSON event stream. A profile can opt a sweep into
parallel execution with `test_threads`:

- unset or `test_threads = 1` - serial, per-test watchdog (the default; nothing
  changes for existing projects).
- `test_threads = 0` - libtest's default parallelism (num_cpus).
- `test_threads = N` (>= 2) - `--test-threads=N`.

A parallel sweep keeps the **same per-test 20s hang watchdog** as the serial
path, with named attribution. Since libtest's human output emits no per-test
*start* signal once tests run concurrently, the parallel path drives libtest's
JSON event stream instead (`--format json -Z unstable-options`, injected
automatically; native on nightly): each `started` event arms the watchdog for
that test, each `ok`/`failed` disarms it, and a test that crosses 20s is blamed
by name and its process group killed - exactly like serial. The JSON events are
reconstructed back into human libtest text so `--raw`/filtered output
all look identical to a serial run. A coarse whole-sweep ceiling (30 min)
remains only as a backstop for an un-attributable wedge (a stall with no test
in-flight - e.g. before the first test starts); it kills the process group and
fails the sweep. This lane is for large workspaces where serial execution is
dominated by a few wall-clock-heavy tests (live/network/multi-second lifecycle)
that parallelism hides - now without surrendering per-test hang protection.
Because the per-test clock is wall-clock, a test that is merely CPU-starved
under heavy `test_threads` load (not hung) can trip it; keep genuinely
multi-second tests off the parallel lane. A profile must not set `--format` in
its `libtest_args` on a parallel lane (the sweep owns that flag). `brokkr test`
is unaffected - it is always serial regardless of the profile's `test_threads`.

### Process isolation (`isolation = "process"`)

A profile may set `isolation = "process"` (see `docs/brokkr.toml.md`): the
sweep's filtered test set is enumerated per test binary (attribution from
`cargo test --no-run --message-format=json`; the binaries run `--list`
directly under the lane's real filter argv - no reimplementation of
libtest filter semantics), package-qualified skips are filtered out of the
enumerated set (with a hard error on a name that exists in both a skipped
and an unskipped package), then each test runs in its own
`cargo test <selection> -- --exact <name> --test-threads=1` invocation.
`--test-threads=1` alone serializes tests within one process per test
binary; it does not isolate them, and tests touching process-global state
(a global logger) need the fresh-process guarantee CI's nextest provides.
Reusing the sweep's selection argv verbatim keeps the build fingerprint
identical across invocations and lets cargo provide the test env
(`CARGO_MANIFEST_DIR`, `OUT_DIR`, …). Each invocation runs under the
standard per-test watchdog. Every test runs even after failures (the
per-test failure list is the point); the sweep fails if any test failed or
if zero tests were enumerated. Shape lines carry `process-isolated`.

Passing tests are **not** listed one per line: a lane of a hundred serial
tests would bury the rest of the run, and the sweep's one summary line
(`serial/live: 52 tests process-isolated passed, 1 pkg-skipped`) carries the
counts. Failures always report in full. `--triage` restores the roll-call: the
pre-run plan line, one `PASS <name> (<secs>)` per test, and a `SKIP` line
per `#[ignore]`d name in a lane without `include_ignored`.

Works without a `brokkr.toml` - usable in any Rust+git repo. When a
`brokkr.toml` is present its host config still applies (e.g. Nidhogg's
`CARGO_TARGET_TMPDIR`); when absent, cwd is the project root.

### Doctests

The test phase does **not** run doctests by default. Every brokkr-managed
project runs its CI under cargo-nextest, which never executes doctests, so a
`brokkr check` that ran them would gate on a signal CI cannot see (a rotten
doctest failing the check, or a passing one masking rot CI ignores). To match
CI, each sweep's `cargo test` is scoped to `--tests` (lib + bins + integration
tests, no doctests) - **unless** the sweep already carries an explicit target
selector (a profile's `--test <name>`, or a `--test`/`--lib`/`--doc`/... token
after `brokkr check --`), which excludes doctests on its own; `--tests` is not
appended on top of one.

Opt a project back in with `[test] doctests = true`, which restores the full
`cargo test` default (doctests included). There is no per-sweep or CLI
override - doctest inclusion is a project-wide, CI-parity property, so it lives
once in `[test]`. `--skip` is not a workaround: doctests share libtest's filter
namespace with unit tests, so skipping them by pattern would eat legitimate
module tests too. `brokkr test <name>` is unaffected - it runs the full
`cargo test` default so a deliberately named doctest still runs.

### Parallel test binaries

A `[[check]]` entry carrying `parallel = { budget = N }` runs its test binaries
concurrently rather than letting cargo run them one at a time, under a budget of
tests in flight across the sweep. It exists for the wall-clock floor: cargo runs
binaries sequentially and `--test-threads` parallelizes only within one, so a
sweep cannot finish faster than the sum, over binaries, of each binary's slowest
test - a floor `test_threads` cannot move.

The sweep builds once, then fans out; each binary claims a slice of the budget
proportional to its serial cost from the previous run - the sum of its own
tests' durations - floored at one slot, capped at its own test count, and
capped again where extra threads stop helping (`serial / slowest_test`). Costs
live in `.brokkr/parallel-timings.toml`; a tree with no history weights by test
count instead, so budget changes want two runs to measure rather than one.
Serial cost rather than wall time because wall time depends on the slots
granted, and feeding that back oscillates.
An unset budget (`parallel = {}`) resolves to the physical cores sharing one
last-level cache - `brokkr env`'s `l3 domain:` line prints the number.

Output is two lines regardless of how many binaries run:

```
[run]     test fanout: 35 binaries, budget 24 in flight (claims 1-3)
[run]     test fanout: 4210 passed in 31.2s (35 binaries, built in 48.9s, slowest broadarrow-worker/ba-worker 28.7s)
```

The `claims N-M` spread is the diagnostic for whether the sweep actually
overlapped. Several binaries each claiming the full budget means they ran one
at a time - the failure mode the proportional claim rule exists to prevent, and
one that otherwise reports success.

A passing binary prints nothing of its own. Thirty-five green lines say only
what the summary says, and a reader who has learned to scroll past the normal
case will scroll past the abnormal one too; `--raw` still emits everything.
Build time and fan-out time are reported separately because a lane sold on wall
time must not fold the compile into the number it is judged on. **The slowest
binary is named because it is the sweep's floor** - the sweep finishes when it
does, so it is the one to split, or to move to the serial lane, and no other
line can say which it was.

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
every invocation would run that target once per planned binary.
Full semantics, and the rule for partitioning a suite across a parallel and a
serial entry, are in `docs/brokkr.toml.md`.

## nextest (bundled; groundwork, not yet selectable)

brokkr links cargo-nextest's engine (`nextest-runner`) rather than shelling out
to a `cargo-nextest` on PATH - the same in-process seam as the elivagar corpus
gate. No per-host install step, and the coverage enumeration reads
`list::TestList` as typed values with no JSON round-trip. The trade is that the
nextest version is brokkr's pin rather than whatever a host happens to have, so
skew against a project's CI nextest is a choice made in `Cargo.toml`.

Motivation is CI parity, and *only* parity. Where a project's CI runs
`cargo nextest run`, brokkr's in-process libtest lanes exercise a shape CI never
runs, and the whole process-global-state class (a global logger installed by two
tests in one binary) is a local-only failure. `isolation = "process"` (see the
`test` phase's process-isolation section) is the small-scale answer to that;
nextest's process-per-test isolation is the general one.

Two things nextest is **not** the answer to, both of which it has been proposed
for:

- **The wall-clock floor.** Process-per-test does dissolve the
  sum-of-per-binary-maxima floor, but so does running the binaries
  concurrently, and that costs no version skew, no change to the coverage key,
  and no default-filter pin. A project whose CI runs no nextest would be
  adopting brokkr's pin as its only exposure to one. See `parallel` in
  `docs/brokkr.toml.md`, which is the answer to the floor.
- **Seeing more bugs locally.** It sees strictly *fewer* of one class. Two
  tests can only contend over a global logger inside one process, so
  process-per-test dissolves the contention along with the bug's visibility. A
  shared-process parallel lane is what *catches* that class; nextest is what
  makes a project need CI parity for it in the first place. For a repo that
  runs no nextest anywhere, moving a sweep onto it retires a detector rather
  than adding one.

No `[[check]]` entry can select the nextest harness yet. What exists is the
coverage key and the disposition classifier (`src/check_cmd/nextest.rs`), landed
first because the coverage pair is the audit's key type. Three behaviours were
measured against cargo-nextest 0.9.143 and are recorded at the code sites that
depend on them:

- **The coverage pair becomes `(binary-id, test)`**, finer than the libtest
  path's `(package, test)`. A package with several test targets has one binary
  id per target, so two integration binaries defining the same test path are two
  pairs rather than one. Package-qualified `[[quarantine]]` and skip entries keep
  their meaning (`package(X)` spans every binary id in X).

  Whether any per-entry **count** moves is empirical and unresolved. It requires
  two binaries in one package to define the same full test path - a stricter
  condition than an entry merely spanning several binaries. nautilus' B51 entry
  does span five binary ids in `nautilus-infrastructure` (four
  `crates/infrastructure/tests/` binaries plus `src/redis/msgbus.rs`, since
  `serial_tests::` is the top-level module name in seven files), so it is the
  candidate; whether those binaries collide on a test path is not knowable from
  the config alone. Settle it before adoption by listing a shape and looking for
  a test path appearing under more than one binary id of the same package - the
  answer is then a number to hand forward rather than a caveat.
- **`MismatchReason` is priority-ordered, not a partition.** A test that is both
  `#[ignore]`d and unmatched by a lane's filterset reports `ignored`, so on a
  single listing it cannot be detected as an orphan. brokkr takes one listing and
  inherits that precedence: the only thing a second `--run-ignored all` listing
  would detect is a test that is both ignored and uncovered, which needs neither
  covering nor quarantining. The scheme self-heals when it matters - remove the
  `#[ignore]` and the verdict flips to `expression`, the pair orphans, and the
  gate fails.
- **The universe must come from its own listing, with `--ignore-default-filter`.**
  A lane listing normally carries every testcase, marking unselected ones
  `mismatch/expression` - but a package-scoped filterset reports the excluded
  package's suite as `skipped` with an empty testcase map, silently shrinking the
  universe. `-E 'all()'` is not a fix: the profile's `default-filter` composes
  with the expression rather than being overridden by it, and a default-filter
  can drop a whole binary (`skipped-default-filter`, empty testcase map) as well
  as individual tests. Lane listings are read only for their selections.

A `default-filter` exclusion is **not** an orphan. It is a third non-fatal
counted bucket alongside `ignored` and `quarantined`: CI does not run those tests
either, so failing on them would make the gate stricter than the CI it exists to
predict, and the only way to clear such a failure would be a quarantine entry for
a test nobody meant to run. It stays distinct from an ordinary filterset mismatch
because the remedy lives in the project's `.config/nextest.toml` rather than in
`brokkr.toml`.

The counted bucket alone does not stop upstream shrinking the audited set
quietly, because a drifting number is not something anyone tracks between runs.
That needs the resolved default-filter **pinned**, so a change reports once, for
a decision. The pin is `(profile, section, raw string)`: the compiled expression
cannot be stored (`CompiledExpr` has no `Display`/`Serialize`, and nextest's raw
accessor is private), but `CompiledDefaultFilter` publicly exposes `profile` and
`section`, and `CompiledDefaultFilterSection` is `Profile` or `Override(usize)` -
the *index* of the override that won. That is a config address, so the string is
read from `profile.<name>.default-filter` or from override #N of
`profile.<name>.overrides` rather than assumed to be the top-level one. No
override-resolution hole survives: editing an override that did not win cannot
move the pin, because the pin records which one did.

Built today: `FilterAddress` and `read_default_filter` resolve an address to its
raw source text, distinguishing "present but sets no filter" (`Ok(None)`, a
legitimate state) from "the address is gone" (an error - it came from nextest's
own resolution, so its absence means the config moved and the pin is meaningless
rather than empty). Still waiting on the lane: producing the address, which needs
a resolved profile, and storing plus comparing the pin, which needs somewhere in
`brokkr.toml` to keep it and a phase to report from.

Filterset translation is mechanical for the existing constructs: substring
`skip`/`only` entries become `test(~X)`, and a `{ package, pattern }` qualified
skip becomes `package(X) & test(~Y)` - which folds the one predicate brokkr
still evaluates itself (libtest has no package scoping) back into the tool.
Note `test(=X)` matches the **full** test path, so an exact-match translation of
a bare test name silently matches nothing.

## `coverage` phase (complete profiles)

Under `certifies = "complete"` a tenth phase, `coverage`, runs after the
test phase - including when the tests **failed**, since the audit needs
built binaries rather than green ones and the orphan worksheet is most
needed on exactly the unhealthy runs. On a failing test phase the audit is
best-effort: its findings print, its counts ride in the JSON, and
`failed_phase` stays `"test"`. Because the test phase fails fast, a lane it
never reached credits **nothing** to the ran-set - the shape still counts
in the universe, so its pairs surface as non-run rather than being counted
as run they never were. The unit of coverage is the **(build shape, package, test)
pair**. `curated = true` entries are the declared narrowing of the
universe (below); package-level `test_exclude_packages` is the other.

The universe is **every `[[check]]` entry**, not the profile's own sweep
list: if it were the latter, dropping a sweep from a lane would silently
shrink the certified set. A `complete` profile that leaves a `[[check]]`
entry referenced by no sweep or lane is therefore a **load-time error**
(the entry would be enumerated nowhere, so the audit would print `0
orphaned` over tests that never ran) - **unless the entry declares
`curated = true`** (see `docs/brokkr.toml.md`): a curated entry's non-run
pairs are outside the universe by declaration, so leaving it unreferenced
certifies nothing without running, and the entry may live in its own
deliberately-run profile instead. When a gate lane does run a curated
entry, its build shape's non-run pairs are exempted rather than audited -
counted and trailer-reported like `test_exclude_packages`, never orphaned,
never credited to a `[[quarantine]]` entry. The exemption is keyed on the
sweeps, not the shape: a non-curated entry sharing a build shape with a
curated one keeps that shape fully audited. Enumeration is per test binary: `cargo test --no-run
--message-format=json` yields each binary with its owning package, then
each binary runs `--list` directly (env-safe: listing executes no test
code). The universe is `--list --include-ignored` with no filters, each
lane's ran-set is `--list` under the lane's real filter argv (libtest
itself decides what an argv admits), package-qualified skips are
subtracted from the lane's claim, and the `#[ignore]`d set comes from
`--list --ignored` (plain `--list` includes ignored names, so a lane
without `include_ignored` has them subtracted from its ran-set). Every
non-run pair must be quarantined (`[[quarantine]]` pattern match,
optionally package-scoped, counted per entry - the **most-specific**
matching pattern takes the pair, so a narrow entry is never starved by a
broad one it nests under) or ignored at the source
(counted, reported, not fatal); anything else is **orphaned** and fails
the check, listed as `shape/package/test` up to `--limit`. A pattern
entry justifying zero pairs is stale and fails the check. A run with
stale entries, orphans and dead filters (below) prints **all** the
worksheets before failing. Package-level
`test_exclude_packages` is outside the pair audit (those binaries cannot
build) and is called out in the trailer.

The ledger reports as **one rolled-up line** - entry count, total pairs,
and the per-issue pair breakdown in descending order (`quarantine: 21
entries, 106 pairs - B51 80, B41 14, B50 10, …`). That keeps both signals
the per-entry listing carried: the countdown, and the growth warning when a
substring starts matching more than it used to. `--triage` prints the old line
per entry, with each entry's pattern and package scope. The `--json` summary carries a
`coverage` object: `pairs`, `run`, `quarantined`, `ignored`, `curated`,
`orphaned`, `dead_filters`. `dead_filters` counts the dead `skip`/`only`
filters below; it exists as its own field because a dead filter moves no
pair between the other buckets - a run that fails on one is otherwise
indistinguishable from a green one in the counts.
It is present whenever the audit got as far as classifying pairs - a run
that fails *on* the audit (stale entries, orphans) still reports its
counts, so a consumer of a failed gate sees the worksheet's numbers and
not `null`. Only an enumeration failure, which predates any counts,
leaves it null.

## Dead `skip` / `only` filters

The same staleness rule the ledger applies to `[[quarantine]]` entries
applies to the filters themselves: **a filter that matches no test is a
defect, not a no-op.** During the `coverage` phase every `skip` and every
`only` declared on a `[test.profiles.*]` block or a `[[check]]` entry is
asserted against the enumeration, and one that matched nothing fails the
check, named with the block it was written in:

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
  narrows binaries with `tests = [...]` (`--test <target>`), so a filter is
  matched against the names *that lane* can see. Against the wider universe
  a skip would read as alive on the strength of a match inside a binary the
  lane narrowed away.
- **The reference set is the filter's own scope**, which is the union of
  the candidate sets of the sweeps it applies to: a `[[check]]` filter
  against the lanes running *that entry*, a `[test.profiles.*]` filter
  against *every sweep the profile runs*. The two claims differ - "this
  test should not run in this sweep" versus "…in this profile" - and the
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

Lanes the test phase never reached are exempt, on the same reasoning that
stops them crediting the ran-set: they ran nothing, so nothing they declare
can be shown dead.

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
cargo test: zero tests ran (sweep: timing) (1 suite(s), 99 filtered out)
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
[[check]] entry 'unit' has `skip` filter "ser", shorter than 4 characters. …
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

## `certifies` and exit codes

A profile may declare `certifies = "complete"` or `"partial"` (see
`docs/brokkr.toml.md`); the claim decides the success word, the exit code,
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
changes with the package set). Trailing `-- …` test args are rejected the
same way under `complete`: a libtest `--skip` or a cargo `--lib` narrows
the real run but not the coverage audit, so the audit would count tests
that never ran. 2 = clap usage errors, 130 = interrupt.

## Per-sweep log lines (collapsed by default)

Each sweep announces itself as `<phase> <name>: <shape>` rather than its full
cargo command:

```
[run]     profile tier1: 3 sweeps (default, ffi, live)
[run]     clippy default: workspace
[run]     clippy ffi: 4 pkgs, +ffi
[run]     clippy live: 2 pkgs, +live
[run]     test default: workspace -2 pkgs, 14 skips, parallel
```

The full command is ~90% profile boilerplate repeated identically per sweep -
on nautilus_trader the three `cargo test` lines are ~1,100 chars each, of which
~900 are the same 14 `--skip` flags, because those come from the *profile*, not
the sweep. What actually varies is package scope and features, which is what
the shape carries. The profile header names the sweep set once; it is printed
only when more than one sweep is active.

The shape is `<package scope>[, <features>][, rustflags …][, <test bits>]`:

- package scope - `workspace`, `N pkgs` (a `packages` list, emitted as `-p`),
  or `workspace -N pkgs` (`test_exclude_packages`; test phase only, since
  clippy stays workspace-wide).
- features - read back out of the flattened argv, so it cannot drift from what
  cargo is handed: `all-features`, `no-default`, `+ffi,live`. A fragment that
  merely restates the sweep's name is dropped (the legacy no-`[[check]]` path
  names its synthesized sweep `all-features`).
- `rustflags <flags> (isolated target)` - always shown, because `rustflags`
  silently redirects the sweep to `target/rustflags-<hash>`, and an unexplained
  full recompile is the one thing a collapsed log must not hide.
- test-phase bits - `N skips`, `include-ignored`, any `--test <name>` filters,
  and the lane (`serial` under the per-test watchdog, `parallel` otherwise).

**Failures always reprint the full command**, as `[error] failing command:
cargo …` - when a sweep fails, the copy-pasteable line is the most useful thing
in the output, so the collapsing applies to success only. This covers clippy
failures, test failures, hung tests, parallel-sweep timeouts, zero-test runs,
and `build_packages` pre-build failures.

`--commands` restores the full command on every line, and additionally logs the
`cargo metadata` invocations of the dependency-rule and publish-cycle phases
(suppressed by default: each is a fixed string that says less than the
`dependency rules: ok (…)` / `publish cycle: ok` line following it). `brokkr clippy` is unaffected and always prints its command: it
is the investigative runner, invoked precisely to find out what a given target
shape does.

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
`skip` / `only` / `tests` / `include_ignored` / `test_threads` / `isolation` /
`env` decide which tests run and how. A test that cannot pass in-process
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

Every ad-hoc run names what it inherited:

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
while the clippy phase dedupes sweeps whose build shape (packages, features,
rustflags, env, build_packages, profile) is identical, logging
`clippy <label>: deduped`. `profile` is in the shape because
`cfg(debug_assertions)` decides which code exists: a dev and a release sweep of
the same features present different lint surfaces, so they are linted
separately rather than deduped into one.

`brokkr test <name>` follows the same ladder except: filters are dropped (the
user's `<name>` argument is the filter), there's no CLI ad-hoc path (the
test runner doesn't accept `--features`), and a lanes profile keeps one
sweep per build shape (with filters dropped, lane duplicates are identical
runs). `--sweep` labels under a lanes profile are the lane-qualified form.

Per-project orchestration blocks (today: `[ratatoskr.harness]`) are **not**
`[[check]]` sweeps and are invisible to both `brokkr check` and `brokkr test`.
They describe how to build a binary that ratatoskr's orchestration commands
(`service`, `mock-serve`, `sync`)
spawn, with their own `package` / `features` / `debug` fields. `[test.profiles]`
may only reference `[[check]]` entries in its `sweeps` list, never an
orchestration block.

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
  `<target>` comes from `cargo metadata --no-deps`. Tests that spawn the
  rebuilt binary should read this var as the primary source of truth and fall
  back to `cfg!(debug_assertions)` only when it's unset (e.g. plain
  `cargo test` outside brokkr). The `cfg!(debug_assertions)` heuristic is
  unreliable because `[profile.test]` overrides can flip
  `debug-assertions = false` in the test binary even though the rebuilt
  binary lives under `debug/`.

## `brokkr test`

`brokkr test [-p <PKG>] <NAME>`. (All cargo projects except litehtml/sluggrs - those are rejected with a
pointer to `brokkr visual`.)

Run one specific cargo test. Defaults to release; pass `--debug` to run the
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

Invokes `cargo test -p <pkg> <name>` (no `--test`), so both unit tests and
integration tests are matched by the name substring within the selected
package.

Package resolution: explicit `-p/--package` > `[test] default_package` in
`brokkr.toml` > `Project::cli_package()` (pbfhogg-cli, nidhogg); workspaces
(e.g. ratatoskr) must pass `-p` or set `default_package`.

Always adds `--include-ignored --nocapture --test-threads=1`.

Sweep selection: if `[test].default_profile` is set, the test runs against
every `[[check]]` entry the profile references (profile filters are dropped -
the user's `<NAME>` is the filter); else if `[[check]]` is non-empty, every
entry runs in declaration order; else fall back to a single `--all-features`
sweep. Each sweep's `build_packages` are rebuilt with the matching feature
flags before the test phase, so `tests/cli_*.rs` invocations get a CLI binary
with the same feature set the test crate sees.

Streams the test's own stdout/stderr live (cargo/test-harness framing lines
are stripped, including the per-suite `Running <target> (.../deps/...)`
launch lines, standalone `ok`/`FAILED` verdict lines, the duplicate
empty `failures:` header, the `RUST_BACKTRACE` hint, and cargo's
`to rerun pass ...` suggestion), then prints a `[test]` footer per run: `PASS`,
`FAIL`, `BUILD FAILED`, or `SKIP`. A sweep `SKIP`s either because the name
didn't match in it (usually `#[cfg(feature = "...")]`-gated) or because the
`-p` target is out of the sweep's package scope - the sweep declares a
`packages` list the target isn't in, or lists the target in
`test_exclude_packages`. The latter is decided *before* the build, so a
target that doesn't carry the sweep's features is skipped rather than
force-built into a guaranteed `BUILD FAILED`. The `FAIL` footer cites the panic message
and location, recovered from the stderr stream since `--nocapture` produces
no captured failure blocks. Exit code: non-zero if any run was
`FAIL`/`BUILD FAILED`, or if *every* sweep was `SKIP` (bad name); `SKIP` mixed
with at least one `PASS` exits `0`.

Flags:
- `-N <n>` - repeat the test (per sweep) for flaky-test hunting. The
  `[run] cargo ...` invocation and build-time lines print for run 1 only.
  The first occurrence of each distinct failure (keyed by panic location)
  prints its full block; repeats of the same failure collapse to their
  `[test] FAIL` footer alone. A closing `[test] summary:` line gives
  PASS/FAIL counts plus one `Nx <msg> @ <loc>` line per distinct failure
- `-j <n>` - cargo `-j N` for parallel compile
- `--raw` - disable all filtering
- `--debug` - dev profile instead of release (overrides both `[test] debug` and
  a sweep's `[[check]] profile`)
- `--release` - force release, overriding `[test] debug = true` and a sweep's
  `profile = "dev"` (mutually exclusive with `--debug`)
- `--timeout <SECS>` - raise the per-test watchdog ceiling (1-280s)

Because `cargo test <name>` is a substring filter, identically-named tests in
different modules of the same package all run; use a more qualified name
(module path) to disambiguate.

A per-test watchdog (shared with `brokkr check`'s test phase) kills any test
that runs longer than 20s and reports it as a hung test. `--timeout <SECS>`
raises that ceiling for `brokkr test` only, and only for a genuinely single
test: each sweep is enumerated with libtest `--list` first, and if `<NAME>`
matches more than one test in any sweep the command errors before running
anything. Sweeps where the name matches zero tests (feature-gated out) are
fine and still `SKIP`. There is no way to disable the ceiling entirely - 280s
is the cap.
