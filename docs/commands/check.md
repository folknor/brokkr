# brokkr check + brokkr test

Both commands share the sweep + profile machinery in `src/profile.rs` and the
test-phase logic in `src/check_cmd.rs`. They differ in scope: `check` is the
full validation pass (gremlins + dependency rules + clippy + tests); `test`
runs one named cargo test against the same sweep set.

For the underlying config (`[[check]]`, `[[dependency_rule]]`, `[test]`
section, profiles) see `docs/brokkr.toml.md`.

## `brokkr check`

Gremlins + dependency rules + clippy + tests. Trailing args after
`brokkr check --` are split on a literal `--`: tokens before it go to
`cargo test` (e.g.
`brokkr check -- --test cli_sort` scopes to one test crate), tokens after go
to libtest after the enforced `--test-threads=1` (e.g.
`brokkr check -- -- --ignored`). With no separator, every token is
cargo-level. The test phase also fails on a successful `cargo test` that ran
zero tests (suites=0, or filters excluded everything) so a too-narrow
profile/filter combo can't silently green-light a check.

Works without a `brokkr.toml` - usable in any Rust+git repo. When a
`brokkr.toml` is present its host config still applies (e.g. Nidhogg's
`CARGO_TARGET_TMPDIR`); when absent, cwd is the project root.

Flags:
- `--features` / `--no-default-features` - ad-hoc sweep, no `build_packages`
- `--profile <NAME>` - selects a `[test.profiles]` entry; conflicts with
  `--features` / `--no-default-features`
- `--raw` - unfiltered cargo output (mutually exclusive with `--json`)
- `--json` - NDJSON diagnostics and summaries on stdout, no prefixed output
- `--limit N` - max diagnostics shown per phase, default 20
- `--all` - show everything, no cap
- `--fix-gremlins` - rewrite banned chars in place before scan

Output:
- Default text mode: each diagnostic becomes one line, compilation noise
  stripped, passing tests aggregated.
- `--json` mode: emits one JSON object per line to stdout. Always emits
  summary events even on success.
- `--raw` reconstructs cargo's terminal-style output by concatenating each
  diagnostic's `rendered` field plus the cargo status messages on stderr -
  one cargo invocation, no separate non-JSON pass.

The clippy phase always invokes cargo with `--message-format=json` and ingests
via `cargo_json::parse_cargo_diagnostics` regardless of output mode - the text
formatter converts each `DiagnosticEvent` into a `ClippyDiagnostic` so every
warning keeps its lint code in the header, even for repeats of the same rule
(cargo's pretty-printed text only annotates the first occurrence per crate,
which is why the JSON ingestion path was needed; see `src/cargo_filter.rs`
module header).

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

Dependency-rule phase runs next only when `[[dependency_rule]]` entries exist
in `brokkr.toml`; without entries it is skipped silently. It reads
`cargo metadata --no-deps` and fails on configured direct dependency boundary
violations, e.g. `from = "app"` with `forbid = "db"` rejects `app -> db`.
JSON mode emits `dependency_violation` and `dependency_summary` events.

When hits exceed `--limit`, both the gremlin and clippy phases prefer files
changed on the current branch (computed via git merge-base against
`@{upstream}` / `origin/master` / `origin/main`) and append a trailer
summarising what's hidden; see `src/scope.rs`.

## `brokkr test [-p <PKG>] <NAME>`

(All cargo projects except litehtml/sluggrs - those are rejected with a
pointer to `brokkr visual`.)

Run one specific cargo test. Defaults to release; pass `--debug` to run the
dev profile instead (faster compile, useful when the failing test isn't
profile-sensitive).

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
`FAIL`, `BUILD FAILED`, or `SKIP` (name didn't match in that sweep, usually
`#[cfg(feature = "...")]`-gated). The `FAIL` footer cites the panic message
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
- `--debug` - dev profile instead of release
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
