I read all of the check-core files, plus `docs/commands/check.md`, `docs/commands/clippy.md`, the relevant parts of `config_parts/schema.rs` and `build.rs`, the check/clippy dispatch in `main_parts/bootstrap.rs`, and the repo's `brokkr.toml`. I ran no shell commands. I haven't verified anything in the other hunters' scopes; where a finding leans on one of those files I say so.

One thing before the list. The repo's own `brokkr.toml` has no `[[script_check]]`, `[[dependency_rule]]`, `[header]`, `[gremlins]` or `[lints]`, contrary to the brief. The only enforcement is three `[[textlint]]` rules plus `[rustdoc]`. It also has no `[[check]]` entries, so brokkr's own gate runs the "legacy" single `--all-features` fallback (`decide_active_sweeps` step 4). What the build can enforce today is therefore textlint rules, unit tests and the type system. Every "enforce by" below uses one of those.

## Bugs I tripped over

1. **`check` swallows the cause of most failures.** `finish_check`'s `Err(_)` arm prints only `check failed in …` and returns `ExitCode(1)`. `main` exits silently on `ExitCode` (`src/main_parts/bootstrap.rs`). The rule that "the failing phase printed its own detail" holds only where a site remembered to print:
   - The test phase re-prints any error that isn't `"tests failed"`.
   - Coverage and `verify_doc_only_rules` print explicitly. The comment at phase.rs ~535 records that this was learned from an earlier incident.
   - Everything else is dropped. That includes both errors in `run_per_build_shape` ("nothing reached {phase}…" and "-p …: every sweep's config rules the selection out"), `project_info` failures, spawn failures, and the IO/metadata errors from gremlins, header, textlint, manifest, dependency-rules and publish-cycle.
   - Example: `[[dependency_rule]].kinds` is documented as "validated when the phase runs". If that validation returns `Err` (I didn't read `dependency_rules.rs`), a typo there prints nothing but `check failed`.
   - **Enforce by:** a `DevError::Reported` variant for "detail already printed"; the summary prints every other variant. That makes the convention structural.
2. **`check` turns a graceful `brokkr kill` into exit 1.** `DevError::Interrupted` also goes through `finish_check`'s `Err(_)` and comes out as `ExitCode(1)`. So `check` never takes main's exit-130 path and never runs the scratch cleanup, even though the documented graceful-kill contract promises both. `cmd_clippy` passes `Interrupted` through correctly.
3. **`brokkr clippy` hides the real error behind "clippy failed".** `cmd_clippy` treats any `DevError::Build` as an already-rendered lint failure. But `build::project_info` returns `Build("cargo metadata failed: …")` and `Build("no Cargo.toml …")`. Run in a non-Rust directory, `brokkr clippy` prints `clippy failed in 0.0s` and never says why. clippy.md claims the opposite.
4. **`brokkr clippy --sweep NAME` doesn't replay the sweep's feature unification.** `build_clippy_sweep` → `sweep_from_check_entry` leaves `effective_unification = Ambient`, and `resolve_unification` is never called. A `feature_unification = "package"` entry is linted as one ambient invocation instead of one per package, and a promoted `auto` parallel sweep isn't pinned to workspace. The `clippy_args` comment names this exact defect, and clippy.md promises "the precise configuration a check sweep lints under". Profile `env` is also absent from the replay; clippy.md admits that part.
5. **The sweep pre-build omits the unification pin.** `run_sweep_pre_build` in output.rs emits the profile, features and `--package`, but not `sweep.unification_args()`. `ResolvedSweep::unification_args` says "every cargo invocation … the pre-build …" must carry it. So in a pinned sweep, `build_packages` binaries are built under a different feature graph from the tests that spawn them. The test that claims to cover this, `a_pinned_unification_reaches_every_compiling_path`, checks only three of the argv builders and skips this one (and `doc_args`).
6. **False "stale entry" warnings from `report_stale_sited_allows`:**
   - `cmd_clippy` passes `packages: &[]` even when ad-hoc `-p` narrowed the run, because the `-p` list lives in `sweep.packages`. So `brokkr clippy -p one` flags every `allow_exact` entry sited in other crates. `--lib` has the same effect on test-only sites.
   - Only clippy results are checked. An `allow_exact = "rustdoc::…@file"` entry can never match a clippy diagnostic, so it warns "suppressed nothing" on every run. check.md covers only the reverse case.
7. **Ad-hoc `--features` can silently drop package-qualified skips.** `profile::run_shaping` → `RunShaping::apply` bypasses the qualified-skip validation in `resolve_single`. A profile whose qualified skips rely on nextest-harness entries hands them to an ad-hoc libtest shared-process sweep, where `qualified_skips` is never read. Tests the profile excludes run anyway.
8. **The `cargo test` failure summary points at a removed flag.** `format_test_failures` in cargo_filter.rs prints "rerun with --raw for the unfiltered output". `--raw` was removed in c9b2776.
9. **Sweep env can defeat the lint-allow plumbing.** `rustflags::sink` inspects only the process environment. A `[[check]]` or profile `env` that sets `RUSTFLAGS` or `CARGO_ENCODED_RUSTFLAGS` is refused at parse time only alongside `rustflags`. Such an env makes cargo read source 1 or 2 while brokkr injects `--config`, which is then inert. On top of that, precedence differs by phase:
   - The test phase (`merged_env`: sweep first, project values appended only if absent) lets the sweep's value win.
   - Clippy and coverage append the brokkr pair after the sweep env, so brokkr's wins (assuming last-set-wins in `run_captured_with_env`, which I didn't read).
10. **The prose-only shortcut's premise is false in this repo.** It rests on "documentation cannot change how the code builds". brokkr `include_str!`s `docs/**.md` into the binary (`brokkr man`), and any crate using `#![doc = include_str!("README.md")]` has doctests and rustdoc output in markdown. A docs-only edit here skips clippy, rustdoc and tests that read those files. Also, the `--json` trailer has no field for the shortcut: a markdown-only run reports `verdict: "passed"`.
11. **A watchdog kill bypasses history and cleanup.** `fire()` calls `process::exit(124)` from the watchdog thread, so `history.db` never records the run, `RunScope` never drops, and the status line isn't cleared. `brokkr clippy` arms no ceiling at all (bootstrap.rs), even though the ceilings exist because of an observed 1h15m clippy hang.
12. **Post-test script checks run on runs whose tests never ran.** `finish_build_phases` runs `PostTest` script checks whenever `test_failure` is `None`. That includes `skip_phases = ["test"]` and the prose-only run, although the comment says they run "only past a green test phase".

## 1. One value, one owner

- **The rustflags env resolution rule has three copies that already disagree:** `rustflags::sink`, `output.rs::composed_rustflags_env` and `phase.rs::announce_invocation_shaping`.
  - For an empty `CARGO_ENCODED_RUSTFLAGS`, `sink` says it is live (`var_os().is_some()`) while `announce` says it isn't (`trim().is_empty()`).
  - A non-UTF-8 encoded value is live to `sink` but falls through to `RUSTFLAGS` in `composed`.
  - All three treat an empty or whitespace `RUSTFLAGS` as unset. As far as I know cargo treats a set-but-empty `RUSTFLAGS` as source 2 (empty flags, config rustflags ignored). If so, `RUSTFLAGS=""` makes every `--config` injection inert, and `sink_may_be_inert` stays silent.
  - **Enforce by:** one `RustflagsEnv::from(env)` value in rustflags.rs, plus a textlint rule forbidding `env::var(_os)?("(CARGO_ENCODED_)?RUSTFLAGS"` outside that file.
- **Phase names are bare `&str` spelled at many sites:** `PHASE_NAMES`, `NON_SKIPPABLE_PHASES`, `begin_phase` literals, the `skip("…")` closure, `phase_ceiling`'s match, and `phase_ok` labels. Those labels already diverge from the identifiers (`"script-check"` vs `script_check`, `"dependency rules"`, `"publish cycle"`). Restated lists live in the ProfileDef doc, check.md's `failed_phase` list and its ceilings table. A typo in `skip("…")` or `phase_ceiling` fails silently (never skipped, or the 2-minute default). **Enforce by:** a `Phase` enum with `as_str`/`label`/`ceiling`; the type makes misspellings unrepresentable.
- **Failure sentinels are strings:**
  - `"tests failed"` is built at phase.rs ~3154 and string-matched at ~544 and ~794; `"coverage failed"` is built in coverage.rs and matched at ~628. `"coverage enumeration failed"` is spelled four times.
  - `"cargo clippy: no issues"` is returned by cargo_filter and compared in output.rs.
  - `replacen("cargo clippy:", "cargo test:")` appears twice (output.rs and `filter_test_build_failure`).
  - **Enforce by:** typed variants; give the text filters a tool-label parameter and have them return `Option` (as `format_clippy_multi` already does).
- **Verdict words, exit codes and the JSON `certifies` strings are spelled inline** in each `finish_check` arm (0/10/1). The `CheckSummary` doc, the `finish_check` doc and check.md restate the pairing. `schema: 1` is a literal. `Certifies` → string is a hand-written match instead of `Serialize`. **Enforce by:** a `Verdict` enum carrying word, JSON and exit code.
- **At least five builders produce a sweep's cargo selection argv**, each ordering profile/unification/packages/excludes/features itself and mixing `--package` and `-p`: `clippy_args`, `doc_args`, `sweep_selection_args`, `shape_selection_args` and `resolution_enumeration_args`, plus the partial copy in `run_sweep_pre_build`. Bug 5 is the divergence this produced. **Enforce by:** one `selection(sweep, scope, Purpose)` function, with a test asserting every call site's argv contains its output.
- **Feature-args construction** is spelled three times: `CheckEntry::cargo_feature_args`, the ad-hoc branch of `decide_active_sweeps`, and `build_clippy_sweep`.
- **`ResolvedSweep` construction:** `sweep_from_check_entry` and `build_resolved_sweep` each list all 25 fields, including a duplicated `lib_only` comment. Three more partial constructors use `..Default::default()`.
- **"Is this a target selector" has two owners:** `has_target_selector` (prefix match, output.rs) and `partition_target_selectors` (parallel.rs, not read). I haven't checked whether they agree. **Enforce by:** a shared table plus a test.
- **Cargo config-chain discovery:** `rustflags::config_paths` owns a `CARGO_HOME` → `HOME/.cargo` fallback. `guard.rs` and `direct_runtime.rs` (`[env]`) must walk the same chain; I didn't read them. Its comment "cargo … prefers `config.toml`" is, to my knowledge, backwards: cargo uses the extensionless `config` when both exist, with a warning.
- **`cargo metadata` runs up to about 8 times per check:** `verify_doc_only_rules`, `resolve_sweep_unification`, clippy, rustdoc, the test phase, coverage, dependency_rules and publish_cycle each call it. There is no single `ProjectInfo` for the run.
- **Duration formatting has two owners:** `fmt_wall` and `lockfile::format_duration`, the latter used by the watchdog.
- **The legacy fallback label `"all-features"`** is a cross-module contract with `brokkr test`, held only by a test comment ("Don't change without updating `brokkr test`").

## 2. Values nobody can find, change, or trust

- **The check tunables are scattered:** `CHECK_CEILING`, `phase_ceiling`, `WATCHDOG_EXIT_CODE` (watchdog.rs); `RUN_LOGS_KEPT` and `.brokkr/check-logs` (report.rs); exit 10 (phase.rs); `PROSE_PHASES`/`PROSE_EXTENSIONS`; the test_runner constants. check.md partly inventories them. None can be injected: `fire()` calls `process::exit`, so no test proves a ceiling fires, and `phase_ceiling` can't be shortened for a test.
- **`rustflags::sink`, `host_triple` (a `OnceLock`) and `config_paths` read the process env and `$HOME` directly**, so `sink` has no unit test at all.
- **Config read at the moment of use:** `composed_rustflags_env` and `announce_invocation_shaping` read the environment per call. `[[dependency_rule]].kinds` is "validated when the phase runs"; combined with bug 1, a typo there may surface as a bare `check failed` (unverified). The qualified-skip check lives in `resolve_single` rather than at load, which is how bug 7 got through.

## 3. One channel, one implementation

- Everything in scope goes through `output::`. There are no raw prints.
- **Pluralisation is ad hoc:** `output::count` exists, yet `format_clippy_multi` spells `error`/`errors` itself, the `dead filter{}` message pluralises manually, and `filter_clippy_in_tree` prints `1 errors` (a test pins that wording).
- **The same class of event goes to different levels:** "skipped" goes to stdout when caused by `-p` and to the log when caused by config. This is deliberate and documented, but it is a policy each call site re-implements.
- **Silent where something significant happens:**
  - the watchdog thread failing to spawn (`.ok()`, so no ceilings);
  - the run log failing to open (best-effort, nothing said);
  - `PHASE_CLOCK` poisoned (the ceiling is skipped silently);
  - the `BuildConfig` sink chosen because an env-configured `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` or `CARGO_BUILD_RUSTFLAGS` exists (not modelled, so no warning).
- **Unbounded lines:** the verdict line lists every `[lints] allow` entry with no bound (the `allow_exact` list was collapsed for exactly this reason). The `invocation:` line echoes all forwarded args.

## 4. Errors

- See bugs 1–3: swallowed failures in `check`, the clippy `Build` conflation, and `Interrupted` mapped to 1.
- `DevError::Verify` is used for test timeouts, a category that says nothing about the subject.
- `resolve_unification` returns `Result` but can never return `Err` (see §8).

## 5. Tests that prove nothing

- **`clippy_sort_key_orders_errors_before_warnings` passes by accident.** The sort key has no level component. It passes only because `"E0308" < "clippy::aaaa"` lexicographically. Production headers are always `error[…]` anyway, so the state under test cannot occur. The comment in `clippy_sort_key` about "the end of their level" is equally stale.
- **`json_summary_tests` build `CheckSummary` by hand** and assert the values they just set. `finish_check`'s verdict, `certifies` and exit mapping are untested.
- **The `allow_exact` tests exercise a test-only composition.** `parse_clippy_from_json`/`gated_diags` (`#[cfg(test)]`) apply only the sited filter. Production applies `keep` = `!is_dependency_warning && !sited_allowed` inside `run_clippy_phase`, so the tests describe a pipeline that never runs.
- **`a_pinned_unification_reaches_every_compiling_path`** claims "every compiling path" but checks three of six builders; the unchecked pre-build is broken (bug 5). `enumeration_selection_carries_the_lint_allows` covers `shape_enumeration_args` but not the package-mode branch of `resolution_enumeration_args`.
- **`merge_clippy_dedups_and_combines_sweeps`** feeds `merge_clippy` from the text parser, while production feeds it from JSON.
- **Environment dependence:**
  - The watchdog tests need Linux `/proc` and a real process tree.
  - `begin_phase_sets_both_bookkeepings` mutates the process-global `PHASE_CLOCK` and leaves it set.
  - `everything_is_displayed_without_git` relies on `/nonexistent` not existing.
  - **Enforce by:** make the clock, ceilings and env injectable (see §7).

## 6. Guards and claims that have stopped holding

**False today:**
- `CheckEntry.harness` and `Harness::Nextest` docs say the engine runs "under the project's own `.config/nextest.toml` (its default profile, retries, timeouts, default-filter)" and "owns build, list and run". check.md says that file "is never opened" and that brokkr owns the compile shape.
- The `Certifies` doc says "until coverage accounting exists".
- The `SkipSpec` doc and `ResolvedSweep.qualified_skips` ("only on process-isolated sweeps, enforced at resolve time") ignore nextest and the ad-hoc path.
- `ResolvedSweep.parallel_budget` says the exclusivity is "enforced at resolve time"; it is actually `reject_conflicting_lanes`, at run time.
- The `ResolvedSweep.build_packages` doc says "`cargo build --release`"; the pre-build uses the sweep profile, dev by default.
- The cargo_filter module doc says the text path is only the "rare build-error case" in `run_test_phase`. In fact it runs on every green test sweep (`filter_clippy_in_tree`), on pre-builds and in `brokkr test`. The `ClippyParse` "errors-first" claim holds only for the text path.
- check.md:
  - "a tenth phase, `coverage`" — the stale count predates rustdoc and install_feature; it's the eleventh.
  - The phase list in the opening lines omits rustdoc and install_feature.
  - The rustdoc section says "capped … like clippy's", contradicting "nothing is capped".
  - "must not set `--format` … on a parallel lane" — `reject_format_override` now applies to every lane.
- `decide_active_sweeps`' doc ("Err only when --profile doesn't resolve") is wrong now that env-merge and run-shaping errors exist.
- The prose premise (bug 10); the `--sweep` fidelity promise (bug 4); the unification-pin promise (bug 5).

**Doc comments attached to the wrong item.** These show up in rustdoc output, since `document_private_items = true`:
- `reject_scoped_complete`'s paragraph sits on `run_sequential_resolutions`.
- `decide_active_sweeps`' doc sits on `CLIPPY_ENV_CONFLICT_REMEDY`.
- `describe_sweep`'s doc sits on `cli_package_scope`.
- `announce_allows`' first paragraph sits on `announce_test_allows`.
- `isolated_target_dir` and `composed_rustflags_env` each fuse two docs.
- A leftover comment in `run_test_phase` mentions a `whole_workspace` binding that no longer exists.

**Stale naming:** several comments still say `[clippy] allow` / `[clippy] allow_exact` (`BuildPhaseArgs`, the `clippy_args` doc, `sited_match`, `gated_diags`). This is cheap to enforce with a textlint rule (`region = "comment"`, pattern `\[clippy\] allow`).

**Guards that fail open on a name:**
- `phase_ceiling`'s `_ => 2` default.
- `skip("literal")`.
- `sink()` ignoring env-configured cargo rustflags and config `include`s.
- `PROSE_EXTENSIONS` treating any `.md` as inert.

**Checkable:**
- the ceilings table vs `phase_ceiling` (a test that parses check.md);
- the `failed_phase` list vs `PHASE_NAMES`;
- operator hints such as `--raw`, `--fix-gremlins`, `--force-rust` and `clean --cargo` — build them from clap, or add a test that runs every hint through `Cli::try_parse_from`, which would have caught bug 8.

## 7. Policy invented per call site

- **"A failure prints its own detail":** see bug 1.
- **The "nothing ran" refusal and `-p` intersection** are implemented in both `run_per_build_shape` and `run_test_phase`, with different messages. `run_per_build_shape`'s doc claims the logic is shared "so they cannot drift".
- **Ceilings** are armed for `check` but not for `clippy`; see bug 11.
- **Poison policy differs between two globals in adjacent files:** `REPORT` recovers via `into_inner`, while `PHASE_CLOCK` silently skips.
- **Globals `REPORT` and `PHASE_CLOCK`** rest on "one run per process", which is asserted, not enforced (`report_begin` silently overwrites).
- **The ambient clock:** run-log names and pruning order use wall-clock milliseconds, so a backward clock step prunes the newest logs.
- **Captured script-check output is persisted verbatim** in `.brokkr/check-logs`. That's worth noting if a script prints env or credentials.
- **Env merge precedence** differs between phases (bug 9).

## 8. Code that is no longer load-bearing

- `profile::resolve_unification` returns `Result` but has no `Err` path left. Its docs say so: the refusal was removed.
- `ProfileDef.description` is `#[allow(dead_code)]` for a "future `brokkr profiles`" command.
- `ClippyDiagnostic.is_error` is constant `true` on the JSON path, and `is_foreign_manifest_warning`'s `is_error` guard only matters for text.
- The `#[cfg(test)]` wrappers `gated_diags`/`parse_clippy_from_json` keep a pipeline shape alive that production no longer uses (§5).
- `ResolvedSweep::libtest_argv` is test-only, with a doc ("Wall-clock-ordered") that describes nothing.
- The "legacy fallback" branch is not dead: brokkr itself runs on it.
