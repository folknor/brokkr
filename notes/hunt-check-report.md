# Design hunt: check-report

Scope: the check phases and what they print - `src/check_cmd/phase.rs`,
`src/check_cmd/output.rs`, `src/cargo_filter.rs`, `src/cargo_json.rs`,
`src/gremlins.rs`, `src/header.rs`, `src/textlint.rs`, `src/script_check.rs`,
`src/rustdoc_check.rs`, `src/rustflags.rs`, `src/scope.rs`, `src/globs.rs`.

Headline: the report layer has no domain types of its own. A phase is a
`&'static str`; a diagnostic's level, lint code and location travel as
strings and get re-parsed out of prose (`"error[code]"`, `"file:l:c"`); the
verdict is a word plus an exit code chosen in parallel branches; "already
printed" is signalled by comparing error message text. Most of the
duplicated decisions below follow from that: with no type to hang a rule on,
each site writes its own.

---

## 1. Axes that should be types

### 1.1 Phase identity is a string, and its attributes are scattered

`PHASE_NAMES: [&str; 13]` and `NON_SKIPPABLE_PHASES` (config_parts/schema.rs),
`PROSE_PHASES` (phase.rs), `phase_ceiling(&str)` (watchdog.rs, a `match` on
names with a `_ => 2` fallback), `begin_phase(_, &'static str)`, the
`skip(&str)` closure, `failing_phase: Option<&'static str>`, and `phase ==
"rustdoc"` branches inside `report_diagnostic_phase` (to pick the cargo
subcommand word) and `report_stale_sited_allows` (to pick the lint namespace
it judges).

Consequences:
- A misspelled literal at any `skip("...")` / `begin_phase(_, "...")` site
  compiles and silently never skips, or reports a `failed_phase` that is not in
  `PHASE_NAMES`. Nothing checks the literals against the list.
- A new phase falls into `phase_ceiling`'s `_ => 2` minute default unless
  somebody remembers that match. The existing test only checks
  `NON_SKIPPABLE_PHASES` is a subset of `PHASE_NAMES`.
- Per-phase facts live in five files: name, skippable, prose-reading, ceiling,
  group (convention vs build), cargo subcommand, which lint namespace it
  judges.

Proposal: `enum Phase { Gremlins, Header, ..., InstallFeature }` in one
module (`check_cmd/phase_id.rs` or alongside the watchdog), with `as_str()`
(the `--json`/config spelling, `FromStr` for `skip_phases`),
`ceiling()`, `skippable()`, `reads_prose()`, `group()`, and for the two
diagnostic phases a `cargo_verb()` / `judges_lint(&LintName)`. `PHASE_NAMES`
becomes `Phase::ALL`; `skip_phases` parses into `Vec<Phase>` at config load;
`failing_phase` and `PHASE_CLOCK` hold a `Phase`; `CheckSummary.failed_phase`
serializes it. Every one of the scattered tables collapses into exhaustive
matches the compiler polices.

### 1.2 Diagnostics: level, code and location are strings, then re-parsed

- `cargo_json::DiagnosticEvent.level: String`, filtered by
  `matches!(level, "error" | "warning" | "error: internal compiler error")`;
  `is_error_level` re-decides with `starts_with("error")`;
  `is_dependency_warning` with `== "warning"`.
- `script_check::Level` already exists as an enum, and
  `cargo_json::manifest_lint_header` converts it *back* into a
  `&'static str` to build a `DiagnosticEvent`.
- `event_to_clippy` renders `code` into `header = "error[{c}]"` and
  `location = "{f}:{l}:{c}"`; then `clippy_sort_key` recovers the code with
  `extract_lint_code` (bracket scan) and the file/line/col with
  `parse_location` (`rsplitn(':')`), and `ClippyDiagnostic::path` splits the
  location string on the *first* `:` (different from `parse_location`'s
  `rsplitn` - a path containing `:` is handled differently by the two).
- `ClippyDiagnostic.is_error` is set `true` unconditionally by
  `event_to_clippy`, so on the JSON path the field carries nothing; it exists
  for the legacy text parser.
- `DiagKey(String, String, String)` is (rendered header, rendered location,
  message) - dedupe identity built from presentation strings.

Proposal: one `Diagnostic { level: Level, code: Option<LintName>, message,
site: Option<Site { file: PathBuf, line: Option<u32>, col: Option<u32> }>,
detail, package: Option<PackageId> }` produced by both the JSON reader and the
manifest-lint reader, and rendered exactly once at the edge. Sort and dedupe
keys derive from fields, not from rendered text. `ClippyDiagnostic` survives
only if the legacy text parser does (see 3.3).

### 1.3 Lint names: namespace decided by prefix in five places

`reaches_test_build` (`!cargo:: && !clippy:: && !rustdoc::`),
`is_cargo_lint` (`cargo::`), `is_rustdoc_lint` (`rustdoc::`, phase.rs),
`sited_match` (strips `clippy::` on either side), `code_allowed` (exact match,
no `clippy::` normalisation). See 2.2 for the disagreement. A
`LintName { tool: Tool /* Rustc|Clippy|Rustdoc|Cargo */, name }` parsed at
config load (for `allow`/`allow_exact`) and at ingestion (for `code`) gives
`reaches_test_build`, "which phase judges this", and "does this allow match
this code" one owner each.

### 1.4 The verdict is a word, a suffix and an exit code chosen in parallel

`finish_check` picks `("passed"|"complete"|"partial", ...,
Ok|Err(ExitCode(10)))` in one match, then `"stopped by its time
ceiling"|"interrupted"|"failed"` plus `ExitCode(124)|Interrupted|ExitCode(1)`
in another, while the `--json` trailer always gets `"failed"` for all three
failure shapes. `emit_json_summary` independently maps `Certifies` to
`"complete"/"partial"`; `TerminationSummary.kind` is a `String` built from
`TerminationCause::as_str()` and `scope` is a `String` that is either a phase
name or the literal `"run"`. The `CheckSummary` doc comment restates the
word/exit-code table by hand.

Proposal: `enum Verdict { Passed { prose_only }, Complete, Partial { narrowed },
Failed(FailureKind /* Error | Deadline | Interrupt */) }` with `word()`,
`json_word()`, `exit()`; `TerminationSummary { kind: TerminationCause, scope:
Scope /* Run | Phase(Phase) */ }` serialized by serde. The doc table becomes
the `exit()` match.

### 1.5 "Already reported" travels as message text

`TESTS_FAILED = "tests failed"` and `CLIPPY_FAILED = "clippy failed"` are
matched by string equality: `already_reported` converts
`DevError::Build(m) if m == TESTS_FAILED` into `Reported`;
`record_phase_stop` matches `Build(m) if m == TESTS_FAILED`; `cmd_clippy`
matches `Reported(m) if m == CLIPPY_FAILED`. Every other phase invents its own
`Reported("gremlins found")`, `"header check failed"`, `"textlint failed"`,
`"manifest check failed"`, `"cargo_env failed"`, `"{phase} failed"`. The
test phase returns a `Build` (meaning "not yet printed") for a failure that
*was* printed, then patches it. Proposal: phases return
`Result<(), PhaseFailure>` where `PhaseFailure` is `Findings { phase }` (detail
printed), `Error(DevError)` (detail not printed), or `Stopped`; the conversion
to `DevError` happens once at `cmd_check`. No caller compares strings.

### 1.6 Smaller ones

- `TestTiming.sweep: String` and `MergedDiag.sweeps: Vec<String>` - sweep
  labels are user strings compared by equality across modules
  (`coverage_tag` matches `reported_by` against `r.label`); a `SweepLabel`
  newtype at least makes them unswappable with package ids, which travel the
  same way (`package_id: Option<String>`, the synthetic
  `"manifest+<path>"` marker minted inline in `attribute_manifest_lints`). The
  marker is a third value of an implicit enum `Owner { Member(id),
  WorkspaceRoot, NonMember(path) }` encoded as a string prefix;
  `is_dependency_warning` then decides by "not in the member map". Make
  `Owner` real.
- `CheckSummary.scope: Option<&str>` only ever `"prose_only"`; `package:
  Option<&str>` is a comma-joined `-p` list ("so the field stays a string").
- `isolated_target_dir` decides naming by `key.starts_with("unify-")` on the
  string `shape_target_key` returned; that key should be an enum
  (`Rustflags(hash) | Unify(..)`) - see 2.5.
- `scope::unstaged_files` returns `Option<HashSet<PathBuf>>` where `None`
  means "not a repo"; `scope::Dirt` already models this properly for the other
  question. A `GitView { tracked, unstaged, status }` (see 3.4) would carry it.

---

## 2. Decisions made in more than one place

### 2.1 "Is this cargo/rustc summary noise, not a finding?"

Sites: `cargo_json::is_summary_noise` (substring: `emitted`+`warning`,
`generated`+`warning`, `aborting due to`, `could not compile`, `build failed`),
`cargo_filter::is_noise` / `is_meta_noise` (the text-path original it was
"ported from"), `phase::is_failure_summary` (prefix `error: could not compile
` or exact `error: build failed`), `phase::is_cargo_status` (prefix `warning:
build failed, waiting for other jobs`, or `) generated <digits> warning`).
They already disagree in shape: one substring-matches, the others anchor on
the header and require digits. Owner: one `rustc_text` module (3.3) with a
single `classify_block(&Block) -> BlockKind { Finding, FailureSummary,
CargoStatus, Progress, ... }` used by both the JSON-message filter (on the
`message`) and the stderr-block readers.

### 2.2 "Does this allow suppress this diagnostic?"

`code_allowed` (exact code equality, used by rustdoc for all codes and by
clippy for `cargo::` codes only) vs `sited_match` (lint equal modulo a
`clippy::` prefix on either side, plus file). So `allow = ["needless_return"]`
is applied by clippy via `-A` (rustc's own normalisation), but an
`allow_exact = "needless_return@src/x.rs"` matches the diagnostic code
`clippy::needless_return` through the strip - while `code_allowed` would not.
Today the difference is masked because `code_allowed` is only consulted for
`cargo::` and `rustdoc::` codes, which have no unqualified spelling. Nothing
keeps that true. Owner: `LintName::matches(&self, code)` (1.3), used by
both.

### 2.3 "Is this a diagnostic about something this workspace does not own?"

`cargo_filter::ClippyDiagnostic::is_foreign_manifest_warning` (text path:
absolute `Cargo.toml` path not under `project_root`) vs
`attribute_manifest_lints` + `is_dependency_warning` (JSON path: package id
not in `cargo metadata`'s `workspace_members`, with the `manifest+` marker for
non-members). They disagree by construction: a path dependency *inside* the
project root but excluded from the workspace is foreign to the second and
owned to the first; a workspace member outside the root (allowed via
`members = ["../x"]`) is the reverse. The test-build failure render
(`output.rs` `filter_clippy_in_tree`) uses the first, clippy/rustdoc the
second. Owner: the `Owner` classification (1.6), fed by metadata, used by both
renderers - which requires the test-build path to stop being text-scraped
(3.3).

### 2.4 "Where does a rustc diagnostic block start?"

`script_check::header_level` (column-zero `error`/`warning`, then `:` or a
*closed* `[...]` followed by `:`) vs `cargo_filter::parse_clippy`
(`starts_with("error:") || starts_with("error[")`, no close or colon check).
`cargo_filter`'s `parse_failed` re-derives the same predicate a third time
inline. A line like `error[oops` opens a block for one and not the other.
Owner: one block splitter (3.3).

### 2.5 "Does this sweep build in its own target dir, and which one?"

`ResolvedSweep::needs_isolated_target_dir` (`!rustflags.is_empty() ||
unification.is_per_package()`, profile.rs) vs `shape_target_key(...).is_some()`
(config_parts/schema.rs), consumed by `isolated_target_dir` (output.rs) which
then decides the directory name by sniffing the key's `"unify-"` prefix, and
by `target_dir_key` (concurrency.rs). `run_per_build_shape`'s comment records
that the predicate already drifted once ("any sweep has rustflags"). Owner:
one `ResolvedSweep::target_location() -> TargetLocation { Shared |
Isolated(Key) }` from which both the "do we need metadata" gate and the path
derive.

### 2.6 "Which layer does a rustflags injection land in?"

`rustflags::sink(build_root, sweep_sets_rustflags)` is the decider, but the
inputs are decided separately per caller: `plumbing` per sweep
(`!sweep.rustflags.is_empty()`) in prepare/test_cmd, a hard `false` in
install_shape, and `announce_test_allows` asks with `sweeps.iter().all(|s|
!s.rustflags.is_empty())` - i.e. it reports the sink for the plain sweeps
only when *every* sweep has rustflags, which is the opposite of its comment
("report the shape the plain sweeps get"). With mixed sweeps it describes
`build_config`/`target_config` correctly by accident; with all plain sweeps
it is right; with all rustflags sweeps it says `RUSTFLAGS`, fine - but the
predicate reads inverted and should be `false` (the plain-sweep question)
explicitly. Separately, `sink`'s soundness argument ("process env plus
`sweep_sets_rustflags` is the whole set of ways source 1/2 can be live")
depends on three independent reserved-name lists: `RESERVED_SWEEP_ENV`
(parser.rs, `[[check]]` env and profile env) and the `[cargo_env]` name
refusal ("belongs to brokkr, cargo, rustc or the loader"). If `[cargo_env]`
ever admits `RUSTFLAGS`, every `--config` injection becomes silently inert.
Owner: one `reserved_cargo_env(name) -> bool` both validators call, cited by
`sink`'s doc.

### 2.7 "How many sweeps is 'multi'?"

`run_clippy_phase`: `multi = sweeps.len() > 1` (configured sweeps).
`run_rustdoc_phase`: `multi = results.len() > 1` (cargo runs, so a single
package-mode sweep with two packages is "multi"). `sweep_tag`'s
`active_sweep_count` is passed `results.len()` from `coverage_tag`, so
`[both]` means "two cargo runs", not "two sweeps": a package-mode sweep plus
nothing else can print `[both]` naming one sweep label twice deduped to one.
`report_diagnostic_phase`'s green line dedups adjacent labels for its count.
Owner: the phase's selection knows the reached-sweep count; pass it, don't
re-derive from vectors of different granularity. Possible bug (unverified by
running): `[both]` on a single package-mode sweep.

### 2.8 "Which files are in the tree?" and "which are dirty?"

`gremlins::tracked_files` (`ls-files --cached --others --exclude-standard`),
`scope::unstaged_files` (`diff --name-only --relative` + `ls-files --others`),
`scope::dirt` (`status --porcelain=v1 --untracked-files=all`). They disagree on
non-UTF-8 paths: `tracked_files` silently drops them (`if let Ok(s)`), so a
non-UTF-8 filename is never scanned by gremlins, header, textlint or manifest -
a gap in exactly the phase that hunts deceptive bytes - while `dirt` treats
one as `Code` (fail-safe) and `git_paths` drops it. Also `dirt` reads
repo-wide status from the project root while the others are root-relative
(`--relative`/cwd), so in a repo whose `brokkr.toml` sits in a subdirectory,
dirt elsewhere in the repo disables the prose shortcut. Owner: one `GitView`
built once per run (3.4), with one policy for unreadable paths (fail closed:
an unreadable path is a finding).

### 2.9 Glob validity

`globs::build_set` is called at *scan* time by header, textlint and manifest,
so an invalid glob in `brokkr.toml` is a phase failure mid-run (after earlier
phases ran) rather than a config-load error; config load decides validity of
everything else. Compile globs in the config parser and store `GlobSet`s.

---

## 3. Structure

### 3.1 `phase.rs` is the whole command

5323 lines: verdict and `--json` schema, sweep activation ladder, profile
resolution, prose shortcut, convention-phase runners and their rendering,
script-check rendering, clippy/rustdoc orchestration, manifest-lint
attribution, allow announcement and staleness, the cross-sweep diagnostic
merger/formatter, failed-run stream excerpting, `cmd_clippy`, the test phase
driver and lane runner, plus 800 lines of tests for all of it. Natural
seams:

- `check_cmd/verdict.rs` - `RunStop`, `finish_check`, `verdict_context`,
  `CheckSummary`, `fmt_wall`, timings. (Owns `Verdict`, 1.4.)
- `check_cmd/conventions.rs` - gremlins/header/textlint/manifest/script/
  dependency/publish-cycle runners, which all share one shape (scan ->
  `Vec<Violation>` -> scope order -> `"<phase>: N violations"` block ->
  `Reported`). Four near-identical bodies (`run_header`, `run_textlint`,
  `run_manifest`, gremlins) differ only in the violation type and the green
  line; a `trait Finding { fn path(&self) -> &Path; fn render(&self) ->
  String }` plus one `report_findings(phase, findings)` removes the per-phase
  copies of the rendering *decision* (header word "found" vs "violation",
  ordering, sentinel).
- `check_cmd/diagnostics/` - the clippy/rustdoc pipeline: `run_per_build_shape`,
  allows (`announce_allows`, stale-allow judging, `sited_match`), manifest-lint
  attribution, merge/format, failed-run stream excerpting (`failed_run_streams`,
  `drop_build_directives`, `is_self_contained`, `failed_stdout_line`).
- `check_cmd/activation.rs` - `decide_active_sweeps`, `build_clippy_sweep`,
  `merge_check_envs` (these are planning; arguably the selection scope's).
- The test phase driver (`run_test_phase`, `run_test_lane`,
  `record_phase_stop`) belongs to the test-execution scope's files, not here.
- `cmd_clippy` next to the diagnostics pipeline it replays.

### 3.2 `check_cmd/output.rs` is not output

It holds test env construction (`build_test_env`, `sweep_cargo_env`,
`sweep_runtime_env`, rustflags composition), isolated target-dir naming,
support-artifact pre-builds and fingerprinting, the serial shim fallback, and
`run_one_test_sweep` (the serial lane, ~400 lines) - 3079 lines, ~1800 of
them tests. Nothing in it is about printing. It is the serial lane's runner
plus the sweep-environment builder. Split into `check_cmd/sweep_env.rs`
(env + target dir, the owner for 2.5) and the serial lane file in the
execution scope; the name `output` should go entirely, since `crate::output`
is the real console module and both are imported side by side.

### 3.3 Three readers of rustc/cargo text, owned by the wrong modules

- `script_check::rustc_blocks` / `Level` / `Block` is the general rustc stderr
  splitter, but lives in the script-check module; `cargo_json` (a JSON parser)
  depends on `script_check` for it, and phase.rs calls it in four places for
  clippy stderr. The dependency points from cargo parsing into a feature
  module.
- `cargo_filter` carries a second block parser (`parse_clippy`), its own noise
  tables, the `ClippyDiagnostic` formatter primitive the JSON path also uses,
  and - unrelated - the whole libtest output parser (`parse_test_output*`,
  `ParsedTestResults`, `Completeness`, panic collection, `filter_test`,
  linker-failure detail). Its module header is stale: it cites
  `src/check_cmd.rs` / `run_test_phase in check_cmd.rs` and "text and JSON
  modes" of test parsing, and says `filter_clippy` survives only for the rare
  test-build-error case - but it is also used by the support pre-build
  (`output.rs` `run_sweep_pre_build`) and `test_cmd`'s clippy-like path.
- The text path's summary line says `cargo clippy: N errors, M warnings` even
  when it is rendering a failed `cargo test` build (`output.rs`
  `filter_clippy_in_tree` on the test build's stderr, `test_cmd`): operator
  text naming the wrong command.

Proposal: `src/rustc_text.rs` (block splitter, `Level`, block classification
- owner of 2.1 and 2.4), `src/cargo_json.rs` (JSON messages -> `Diagnostic`,
plus manifest lints read via `rustc_text`), `src/diagnostic.rs` (the
`Diagnostic` type, `Owner`, `LintName`, rendering), and move libtest parsing
to the test-execution scope (`test_runner/libtest_text.rs` or similar). Then
retire `parse_clippy` entirely: run the test build and the support pre-build
with `--message-format=json` (the serial lane already parses cargo JSON for
executables in `build.rs`) and render through the same pipeline as clippy.
The header's reason for keeping the text path ("not worth a second JSON
pass") no longer holds once the formatter is shared; keeping it is what makes
2.3 and 2.4 possible.

### 3.4 File-walking convention scanners each re-walk the tree

`gremlins`, `header`, `textlint`, `manifest` each call
`gremlins::tracked_files` (one `git ls-files` each) and `read_in_scope` on
their own files; the shared file universe is owned by `gremlins`, which
header/textlint/manifest import for it. `scope` runs two or three more git
commands. A `TreeSnapshot` (tracked+untracked paths, unstaged set, dirt) built
once per `check` in a `tree` module, handed to every convention phase, with
file contents read once and shared, gives: one git call, one unreadable-path
policy (2.8), one owner that isn't gremlins, and a natural place to run the
four scanners in one pass. `scope::prioritize` then becomes a method on it.

### 3.5 Reporting decisions inside execution helpers

`run_per_build_shape` both schedules cargo runs and decides what to print for
skipped/deduped/dropped lanes (`note`), switching between stdout and the run
log on `report_active()` - a process-global set by `check`. `announce_allows`
takes two bools (`to_stdout`, `blanket_on_stdout`) and also consults
`report_active()`; `run_clippy_phase` passes `commands || !report_active()`
and `!echo_after`. "Who is the audience for this line" is decided by a global
plus ad-hoc booleans at each site. A `Reporter` value (`Check { commands } |
Clippy`) passed explicitly, with methods `config_note`, `allow_notice`,
`green_line`, makes the `check` vs `brokkr clippy` difference one type
instead of a global and four flags.

### 3.6 `finish_check` and friends take 13 positional arguments

`#[allow(clippy::too_many_arguments)]` appears on `finish_check`,
`emit_json_summary`, `report_diagnostic_phase`, `run_per_build_shape`,
`run_rustdoc_phase`, `cmd_clippy`, and others. `RunReport` already exists for
half the verdict state; the rest (`certifies`, profile label, sweep labels,
lints, skip phases, prose-only, package, failing phase, started) is the
`Verdict` input and should be one struct assembled as the run proceeds rather
than threaded at the end.

---

## Lateral findings

- `tracked_files` drops non-UTF-8 paths silently (2.8): gremlins cannot see a
  file whose name is itself the deceptive bytes. Fail closed.
- `announce_test_allows`'s sink query predicate reads inverted vs its comment
  (2.6).
- Possible `[both]` mislabel from `results.len()` used as sweep count (2.7).
- `cargo clippy:` header on test-build failure text (3.3).
- `cargo_filter` module header cites files and call sites that moved (3.3).
- `script_check::evaluate` lossily decodes stdout/stderr and, for
  `Stream::Both`, joins them with `\n` - so `MatchMode::Exact` on `both` can
  never match a sentinel unless stderr is empty and the config `expect` ends
  where stdout does; `last-line` on `both` reads stderr's last line, not the
  last line printed. Interleaving is lost; worth documenting or capturing a
  merged stream.
- `gated_diags` and `parse_clippy_from_json` are `#[cfg(test)]` copies of the
  production gating path ("this whole-stream form remains for the parser
  tests"): tests exercise a parallel form, not the `keep` predicate the phases
  use.
- `header::current_utc_year` via libc `gmtime` is called per run inside the
  phase; if it fails the header phase fails with a time error - fine, but the
  year is also exactly the kind of input a test would want injected.
