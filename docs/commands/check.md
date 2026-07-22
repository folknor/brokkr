# brokkr check + brokkr test

Both commands share the sweep + profile machinery in `src/profile.rs` and the
test-phase logic in `src/check_cmd.rs`. They differ in scope: `check` is the
full validation pass (gremlins + style + header + textlint + dependency rules +
clippy + tests); `test`
runs one named cargo test against the same sweep set.

For the underlying config (`[[check]]`, `[[dependency_rule]]`, `[test]`
section, profiles) see `docs/brokkr.toml.md`.

## `brokkr check`

Gremlins + style + header + textlint + dependency rules + clippy + tests. Trailing args after
`brokkr check --` are split on a literal `--`: tokens before it go to
`cargo test` (e.g.
`brokkr check -- --test cli_sort` scopes to one test crate), tokens after go
to libtest after the default `--test-threads=1` (e.g.
`brokkr check -- -- --ignored`). With no separator, every token is
cargo-level. The test phase also fails on a successful `cargo test` that ran
zero tests (suites=0, or filters excluded everything) so a too-narrow
profile/filter combo can't silently green-light a check.

### Per-sweep rustflags + auto target-dir isolation

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

### Serial vs parallel test sweeps

By default the test phase runs each sweep serial (`--test-threads=1`) under
the per-test hang watchdog (see below), attributing a stall to a named test
from libtest's sequential output. The parallel lane keeps the same watchdog via
libtest's JSON event stream (see below). A profile can opt a sweep into
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

Like every locked brokkr command, `check` and `test` acquire the global
per-user lock **blocking**: if another brokkr invocation (e.g. a bench run)
holds it, the command prints `[lock] waiting for …` and waits until released,
then proceeds - rather than failing with `lock: already locked`. So a
concurrent lock never produces an error to handle; just let the command wait.

Flags:
- `--features` / `--no-default-features` - ad-hoc sweep, no `build_packages`
- `--profile <NAME>` - selects a `[test.profiles]` entry; conflicts with
  `--features` / `--no-default-features`
- `--raw` - unfiltered cargo output (terminal-style rendering)
- `--json` - append one machine-readable summary line (a JSON object) as the
  last line of stdout; human output is unchanged
- `--limit N` - max diagnostics shown per phase, default 20
- `--all` - show everything, no cap
- `--fix-gremlins` - rewrite banned chars in place before scan
- `--commands` - log each sweep's full cargo command instead of the collapsed
  form (see below)

Output:
- Default text mode: each diagnostic becomes one line, compilation noise
  stripped, passing tests aggregated.
- `--raw` reconstructs cargo's terminal-style output by concatenating each
  diagnostic's `rendered` field plus the cargo status messages on stderr -
  one cargo invocation.
- `--json` appends one summary object as the **last line of stdout**, leaving
  the human output untouched (the old NDJSON per-event mode is gone; this is
  the TIERED-CHECK.md feature-8 result contract). Fields: `schema` (currently
  1), `certifies` (always `null` until profile certification exists),
  `verdict` (`"passed"`/`"failed"`), `profile` (the profile that drove sweep
  selection; `null` for ad-hoc and legacy runs), `sweeps` (labels),
  `failed_phase` (`null` on success, else one of `gremlins`/`style`/`header`/
  `textlint`/`manifest`/`script_check`/`dependency_rules`/`clippy`/`test`),
  `elapsed_ms`. The object is versioned and additive: fields are only ever
  added under `schema: 1`, consumers must tolerate unknown fields, and a bump
  is reserved for renames or semantic changes. A config error before the
  phases run (bad profile name, conflicting flags) emits no summary -
  resolve-time errors are not run verdicts. Exit codes remain 0 (pass) / 1
  (fail); 10 is reserved for a future partial pass under `certifies` and
  verified free in check's namespace (2 = clap usage errors, 130 =
  interrupt).

### Per-sweep log lines (collapsed by default)

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
dependency-rule phase's `cargo metadata` invocation (suppressed by default: it
is a fixed string that says less than the `dependency rules: ok (…)` line
following it). `brokkr clippy` is unaffected and always prints its command: it
is the investigative runner, invoked precisely to find out what a given target
shape does.

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

Gremlin phase runs first and fails the check if any banned Unicode character
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

Style phase runs next, only when `[style]` enables a rule (off by default, so
it is inert for every project that does not opt in). The one current rule,
`rust_blank_line_above_control_flow`, requires a blank line above
`if`/`match`/`for`/`while`/`loop`/`spawn` constructs, with an exemption ladder
(first expression in a block, comment/attribute above, string continuation,
shared identifier with the line above or the first body line, plus per-keyword
carve-outs: else-if chains, expression position, loop labels, `.spawn` method
chains). It scans tracked `.rs` files, honouring `[gremlins].exclude`. JSON
mode emits `style` and `style_summary` events. Ported from nautilus_trader's
`check_formatting_rs` hook; see `src/style.rs`.

Header phase runs next, only when a `[header]` section is present. A file
matching `[header].paths` (minus `exempt`) must contain `[header].pattern` with
`{year}` expanded to the current UTC year; a missing header or a stale year
fails. JSON mode emits `header`/`header_summary`. Ported from
`check_copyright_year`; see `src/header.rs`.

Textlint phase runs next, only when `[[textlint]]` rules exist. Each rule
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
attribute) so a rustfmt-wrapped `#[cfg(...)]` still suppresses. JSON mode emits
`textlint`/`textlint_summary`. The generic engine behind most grep-style
convention hooks; see `src/textlint.rs`.

Manifest phase runs next, only when a `[manifest]` section enables a check
(off by default, inert otherwise). It parses each `Cargo.toml` matching
`[manifest].paths` (minus `exclude`) with `toml_edit` and enforces structural
conventions - today `sort_dependencies` (dependency keys sorted within each
blank-line group; `[dependencies.<name>]` dotted sections, which TOML forces
physically after the inline table, are their own group and never ordered against
it). `shape_exclude` globs excuse a manifest from the structural checks only
(section/crate-type/package-field order, `[lints] workspace`, bin/example flags
- the same set a `cargo-fuzz = true` stub skips) while still sort-checking it;
`exclude` skips the file entirely. JSON mode emits `manifest`/`manifest_summary`.
See `src/manifest.rs`.

Script-check phase runs next, only when `[[script_check]]` entries exist (inert
otherwise). Each entry runs `command` via `sh -c` (so pipes/redirects/env
expansion work) with cwd = the code tree, and **passes iff the captured output
matches `expect`**. Asserting on a success sentinel - not the exit code - is the
point: it catches a check silently stubbed to `exit 0`, because the script must
prove it ran to completion by emitting the sentinel. The command's exit code is
therefore ignored; only a spawn failure is a hard error. Every entry runs (no
fail-fast within the phase) so one `brokkr check` surfaces all broken gates, and
each failure prints the full captured stdout/stderr (the diagnostic, never
truncated by `--limit`). It fills the gap for gates brokkr's native phases can't
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

See `src/script_check.rs`.

Dependency-rule phase runs next only when `[[dependency_rule]]` entries exist
in `brokkr.toml`; without entries it is skipped silently. It reads
`cargo metadata --no-deps` and fails on configured direct dependency boundary
violations, e.g. `from = "app"` with `forbid = "db"` rejects `app -> db`. A rule
can scope the forbidden match by dependency `kinds` (`normal`/`dev`/`build`,
default all) and `optional` (e.g. `optional = false` to require a dep be
optional), so manifest conventions like "tokio only as a dev-dependency" are
expressible. JSON mode emits `dependency_violation` and `dependency_summary`
events.

When hits exceed `--limit`, both the gremlin and clippy phases prefer files
changed on the current branch (computed via git merge-base against
`@{upstream}` / `origin/master` / `origin/main`) and append a trailer
summarising what's hidden; see `src/scope.rs`.

## `brokkr test [-p <PKG>] <NAME>`

(All cargo projects except litehtml/sluggrs - those are rejected with a
pointer to `brokkr visual`.)

Run one specific cargo test. Defaults to release; pass `--debug` to run the
dev profile instead (faster compile, useful when the failing test isn't
profile-sensitive). Setting `[test] debug = true` in `brokkr.toml` flips the
default to dev; `--release` forces release back. Precedence: `--debug` /
`--release` (mutually exclusive) > `[test] debug` > release.

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
- `--debug` - dev profile instead of release (overrides `[test] debug`)
- `--release` - force release, overriding `[test] debug = true` (mutually
  exclusive with `--debug`)
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

## Sweep selection table (`brokkr check`)

| invocation | sweep set | libtest filters |
|---|---|---|
| no `[[check]]`, no flags | one `--all-features` sweep (legacy default) | none |
| `[[check]]` configured, no `default_profile`, no flags | every `[[check]]` entry in declaration order | none |
| `[[check]]` + `default_profile = "tier1"`, no flags | the entries `tier1.sweeps` references | tier1's filters |
| `--profile tier1` | the entries `tier1.sweeps` references | tier1's filters |
| `--features X` (or `--no-default-features`) | one ad-hoc sweep, no `build_packages` | none |

`brokkr test <name>` follows the same ladder except: filters are dropped (the
user's `<name>` argument is the filter), and there's no CLI ad-hoc path (the
test runner doesn't accept `--features`).

Per-project orchestration blocks (today: `[ratatoskr.harness]`) are **not**
`[[check]]` sweeps and are invisible to both `brokkr check` and `brokkr test`.
They describe how to build a binary that ratatoskr's orchestration commands
(`service-test`, `service-suite`, `mock-serve`, `sync-smoke`, `sync-bench`)
spawn, with their own `package` / `features` / `debug` fields. `[test.profiles]`
may only reference `[[check]]` entries in its `sweeps` list, never an
orchestration block.

## Env vars exported to `cargo test`

Both `brokkr check` (test phase) and `brokkr test` set the following on every
`cargo test` invocation, including sweeps with empty `build_packages`:

- `BROKKR_TEST_BIN_DIR` - directory containing the just-rebuilt
  `build_packages` artefacts. `brokkr check` always sets it to
  `<target>/debug` (the test phase runs without `--release`); `brokkr test`
  sets it to `<target>/release` by default and `<target>/debug` when
  `--debug` is passed. The profile tracks the cargo invocation 1:1 - it does
  *not* track whatever profile cargo happens to compile the test harness with.
  `<target>` comes from `cargo metadata --no-deps`. Tests that spawn the
  rebuilt binary should read this var as the primary source of truth and fall
  back to `cfg!(debug_assertions)` only when it's unset (e.g. plain
  `cargo test` outside brokkr). The `cfg!(debug_assertions)` heuristic is
  unreliable because `[profile.test]` overrides can flip
  `debug-assertions = false` in the test binary even though the rebuilt
  binary lives under `debug/`.
