# Hygiene - validation pipeline

Hygiene findings from the hunt for the `check`/`test`/`clippy` pipeline, test execution, the convention engines (gremlins, header, textlint, script_check, manifest, dependency rules) and `brokkr deps`. Siblings: `hygiene-platform.md` (PLT, cross-cutting themes), `hygiene-measurement.md` (MEA), `hygiene-projects.md` (PRJ), `bugs.md` (BUG). Entries record what hunters reported; nothing has been verified.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## VAL-001 - The rustflags env resolution rule has three copies that already disagree

Reported by: check-core.

`rustflags::sink`, `check_cmd/output.rs::composed_rustflags_env` and `phase.rs::announce_invocation_shaping` each restate which of `CARGO_ENCODED_RUSTFLAGS`/`RUSTFLAGS` is live. Empty `CARGO_ENCODED_RUSTFLAGS`: live to `sink` (`var_os().is_some()`), not live to `announce` (`trim().is_empty()`). A non-UTF-8 encoded value is live to `sink` but falls through to `RUSTFLAGS` in `composed`. All three treat empty `RUSTFLAGS` as unset (see BUG-009). `composed`/`announce` read the environment per call. `sink` ignores env-configured `CARGO_TARGET_<TRIPLE>_RUSTFLAGS`/`CARGO_BUILD_RUSTFLAGS` and config `include`s, silently; it has no unit test because it reads env and `$HOME` directly.

Enforcement proposed: one `RustflagsEnv::from(env)` in `rustflags.rs` plus a textlint rule forbidding `env::var(_os)?("(CARGO_ENCODED_)?RUSTFLAGS"` outside it.

## VAL-002 - Phase names are bare strings at many sites

Reported by: check-core.

`PHASE_NAMES`, `NON_SKIPPABLE_PHASES`, `begin_phase` literals, the `skip("…")` closure, `phase_ceiling`'s match (`_ => 2` default), `phase_ok` labels. Labels already diverge from identifiers (`"script-check"` vs `script_check`, `"dependency rules"`, `"publish cycle"`). Restated in the `ProfileDef` doc, check.md's `failed_phase` list and its ceilings table. A typo in `skip("…")` or `phase_ceiling` fails silently (never skipped, or the 2-minute default).

Enforcement proposed: a `Phase` enum with `as_str`/`label`/`ceiling`; tests parsing check.md's ceilings table and `failed_phase` list against it.

## VAL-003 - Failure sentinels and tool labels are strings matched by content

Reported by: check-core, test-execution.

- `"tests failed"` built in phase.rs and string-matched twice; `"coverage failed"` built in coverage.rs and matched in phase.rs; `"coverage enumeration failed"` spelled four times.
- `"cargo clippy: no issues"` returned by cargo_filter and compared in output.rs.
- `filter_clippy` output relabelled with `replacen("cargo clippy:", "cargo test:")` (output.rs, `filter_test_build_failure`) and `"cargo build:"` (test_cmd).

Enforcement proposed: typed variants; text filters take a tool-label parameter and return `Option` (as `format_clippy_multi` does).

## VAL-004 - `check`'s verdict, exit code and JSON `certifies` are spelled inline per arm

Reported by: check-core.

Each `finish_check` arm spells verdict word, exit code (0/10/1) and `certifies` string; the `CheckSummary` doc, `finish_check` doc and check.md restate the pairing; `schema: 1` is a literal; `Certifies` → string is a hand-written match instead of `Serialize`. `finish_check`'s verdict/certifies/exit mapping is untested (VAL-021). See PLT-028 for the repo-wide exit-code table.

Enforcement proposed: a `Verdict` enum carrying word, JSON and exit code.

## VAL-005 - A sweep's cargo selection argv is built by several independent builders

Reported by: check-core, test-execution.

- `clippy_args`, `doc_args`, `sweep_selection_args`, `shape_selection_args`, `resolution_enumeration_args`, plus a partial copy in `run_sweep_pre_build`, each order profile/unification/packages/excludes/features themselves and mix `--package`/`-p`. BUG-005 is the divergence this produced.
- `test_cmd` re-derives profile → subdir and `--release` from a bool (`"debug"`/`"release"`) in `run`, `test_argv`, `matching_test_names`, `run_pre_build`, instead of `SweepProfile::target_subdir`/`cargo_args`.
- `sweep_selection_args` already includes profile args and `unification_args()`; the nextest build and parallel prebuild append `unification_args()` again and nextest appends profile args again (harmless today; `parallel.rs` claims the pin comes "from that one field").
- `enumeration_selection_carries_the_lint_allows` covers `shape_enumeration_args` but not the package-mode branch of `resolution_enumeration_args`.

Enforcement proposed: one `selection(sweep, scope, Purpose)` / `LibtestInvocation` builder carrying profile, pin, allows and selection, with a test asserting every call site's argv contains its output.

## VAL-006 - Feature args and `ResolvedSweep` construction are repeated

Reported by: check-core.

Feature args are built in `CheckEntry::cargo_feature_args`, the ad-hoc branch of `decide_active_sweeps`, and `build_clippy_sweep`. `sweep_from_check_entry` and `build_resolved_sweep` each list all 25 fields (with a duplicated `lib_only` comment); three more partial constructors use `..Default::default()`.

## VAL-007 - Cargo/libtest argv classification is re-implemented per lane

Reported by: check-core, test-execution.

- `narrows_selection` (`-p`, `--package`, `--exclude`); `reject_forwarded_selectors` (same plus `--workspace`); `partition_target_selectors` and `reject_unsupported_forwarded` each re-split on `=`.
- "Is this a target selector": `has_target_selector` (prefix match, output.rs) and `partition_target_selectors` (parallel.rs); agreement unchecked.
- `--format` refusal: output.rs checks both spellings on the final argv; `direct_libtest_args` catches `--format` not `--format=`; nextest refuses all libtest args.

Enforcement proposed: one `CargoArgv` classifier type with a shared selector table and test.

## VAL-008 - libtest `--list` parsing, loader paths and cwd fallback have multiple owners

Reported by: test-execution.

- `is_list_tally` duplicated verbatim in `test_cmd.rs` and `isolate.rs`; name extraction diverged: `listed_test_names` uses `trim_end` and doesn't dedup, `parse_list_output` trims both ends, sorts and dedups.
- `binaries::loader_path` (listing) orders libdir, deps, profile, inherited with no build-script link-search paths; `DirectRuntime::envelope` (execution) orders linked, deps, profile, libdir, inherited and says order "decides which same-named .so loads"; they read an existing `LD_LIBRARY_PATH` first-match vs last-match. A binary needing a build-script `.so` can run but fail to list.
- cwd fallback: `envelope` returns `"."` (and `CARGO_MANIFEST_DIR="."`), `run_one_binary` maps `"."` to `project_root`, `binary_list` falls back to `project_root` itself.

Enforcement proposed: listing goes through `envelope`; a test asserts listing and execution environments are built by one function.

## VAL-009 - Two owners of `-Zfeature-unification` support; package-mode builds in two target dirs

Reported by: test-execution.

`install_shape::is_toolchain_refusal` records that matching `contains("nightly")` was wrong and fixed it; `binaries::test_binaries_with_runtime` still uses `contains("feature-unification") || contains("nightly")`. The install-feature phase builds package-mode in the main target dir, while a `feature_unification = "package"` test lane is isolated to `unify-package` because sharing "would thrash against every ordinary sweep"; the two never share a cache and the install phase contradicts that rationale.

## VAL-010 - Test time limits are restated by hand

Reported by: test-execution, check-core.

- 280 has no named owner: only clap `range(1..=280)`, help text and docs.
- "20s" hand-written in the `test_cmd` error string ("run them all at the 20s ceiling"), CLI help, the `test_cmd` header, and 15+ times in `check.md`.
- `IDLE_TIMEOUT`'s comment says it "matches check's clippy ceiling", untied to `watchdog::phase_ceiling`.
- `SWEEP_WALL_TIMEOUT` and `PARALLEL_SWEEP_TIMEOUT` (both 1800s) enforced two ways (watchdog `wall` vs `try_wait` loop); inside `check` the 15-minute phase ceiling always fires first, so the parallel `timed_out` branch and its "(1800s)" messages are unreachable (VAL-028).

Enforcement proposed: name `MAX_TEST_TIMEOUT` and use it in the parser; format messages from constants; a unit test asserting `check.md` (which is `include_str!`'d) states `TEST_TIMEOUT.as_secs()`.

## VAL-011 - Check/test tunables have no index and no injection point

Reported by: check-core, test-execution, config-cli-bootstrap.

- `CHECK_CEILING`, `phase_ceiling`, `WATCHDOG_EXIT_CODE` (watchdog.rs); `RUN_LOGS_KEPT`, `.brokkr/check-logs` (report.rs); exit 10 (phase.rs); `PROSE_PHASES`/`PROSE_EXTENSIONS`; four `test_runner.rs` constants plus `WATCHDOG_POLL`; `cpu_topology` fallback 4 (also in `tools.rs`). check.md partly inventories them.
- `fire()` calls `process::exit`, so no test proves a ceiling fires and `phase_ceiling` can't be shortened for a test.
- `cpu_topology::CPU_ROOT` is hard-coded, so `physical_cores` is untestable; the count ignores affinity and cgroup cpusets, so in a restricted container the default budget exceeds usable CPUs.
- `ParallelBinaries::resolved_budget` claims `brokkr env` "reports the same figure" and that it is "resolved at config load"; `env` shows `cache_domain_cores()` without the `_or_default` fallback ("not detected" while the lane runs at `available_parallelism` or 4), and it is called from `profile.rs`. The same doc still describes the replaced claim rule ("A binary claims `min(its test count, budget)` slots").

## VAL-012 - `NEXTEST_ENGINE_VERSION` is a hand copy of Cargo.toml

Reported by: test-execution.

`"0.124.0"` is copied by hand; the dependency is a caret requirement, so `cargo update` can move the lock to 0.124.x without the constant following. Matches today. Enforcement proposed: a test parsing `include_str!("../Cargo.lock")`, or an `=` pin.

## VAL-013 - Two libtest runners differ by accident

Reported by: test-execution.

`streaming_run_libtest` and `run_libtest_parallel` share the tracker but differ in: wall clock (watchdog `wall` vs `try_wait`), whether a wall kill snapshots `/proc` (serial yes, parallel no), cancellation (abort and shutdown checks only in parallel), `build_elapsed` tracking (serial only), and the timeout message. Hunter's view: one runner taking `Ceilings`, an optional abort flag and a program.

## VAL-014 - nextest engine setup is copied twice and has diverged

Reported by: test-execution.

`run_nextest_sweep` and `nextest_shape_cases` each hold ~120 identical lines (host detection, metadata, configs, build, builder, synth config, profile, ctx, `TestList`); only the audit path has the `build-finished` guard.

## VAL-015 - The same condition is handled differently across test lanes

Reported by: test-execution.

- Doctests on an isolated lane warn every run; the parallel lane refuses at config load, and its own comment explains why a per-run warning is wrong.
- Forwarded args: refused by isolated, partitioned by parallel, cargo-only for nextest.
- Target runners: parallel refuses, nextest honours, and nextest drops an unparseable one silently (`TargetRunner::new(...).unwrap_or_else(|_| TargetRunner::empty())`).
- `isolation = "process"` and `harness = "nextest"` are two implementations of process-per-test; the docs already call nextest the concurrent form of the same guarantee.
- `enforce_single_threaded` and the serial lane's second `--test-threads=1` requirement are leftovers of the partial-marker machine; the JSON tracker handles concurrency, which `run_libtest_parallel` proves (checkable).

## VAL-016 - Check run-state policy is per call site

Reported by: check-core.

- The "nothing ran" refusal and `-p` intersection are implemented in both `run_per_build_shape` and `run_test_phase` with different messages; `run_per_build_shape`'s doc claims the logic is shared "so they cannot drift".
- Globals `REPORT` and `PHASE_CLOCK` rest on "one run per process", asserted not enforced (`report_begin` silently overwrites); poison policy differs between them (`REPORT` recovers via `into_inner`, `PHASE_CLOCK` silently skips the ceiling).
- Run-log names and pruning order use wall-clock ms, so a backward clock step prunes the newest logs.
- Captured script-check output is persisted verbatim in `.brokkr/check-logs` (see PLT-037).

## VAL-017 - The legacy fallback label `"all-features"` is a cross-module contract held by a comment

Reported by: check-core.

Shared with `brokkr test`, held only by a test comment ("Don't change without updating `brokkr test`"). brokkr itself runs on this fallback (no `[[check]]` entries in its `brokkr.toml`).

## VAL-018 - Test lanes read cargo config late and fail open

Reported by: test-execution.

`cargo_config_env` and `refuse_configured_runner` silently skip cargo config files that don't parse, so the runner refusal fails open on a malformed file despite its "fail closed" rationale. The hunter suspects `CargoConfigs::new` discovers config from the process cwd, not `project_root` (verify).

## VAL-019 - Fixed shared files rewritten non-atomically every run

Reported by: test-execution, process-control.

`nextest-synth.toml` and `~/.brokkr/nextest-env.toml` are rewritten non-atomically on every run, safe only because of the global lock; `nextest-env.toml` is also written capability-less outside a hold, which that rationale doesn't cover.

## VAL-020 - Test-execution resources grow without bound

Reported by: test-execution.

`.brokkr/test-hung/<ts>-<pid>-<name>/` snapshots are never cleaned and `clean.md` doesn't mention them; the parallel lane spawns one OS thread per planned binary up front plus three per running binary.

## VAL-021 - Check/test tests that cannot fail

Reported by: check-core, test-execution.

- `clippy_sort_key_orders_errors_before_warnings` passes only because `"E0308" < "clippy::aaaa"`; the key has no level component and production headers are always `error[…]`. The `clippy_sort_key` comment about "the end of their level" is stale.
- `json_summary_tests` build `CheckSummary` by hand and assert what they set.
- `a_pinned_unification_reaches_every_compiling_path` claims "every" but checks three of six builders (BUG-005).
- `test_cmd::decide_sweeps_default_profile_filters_dropped` asserts only the label; its comment ("Sweep struct only carries label / feature_args / build_packages") is false.
- `binary_timings::serial_cost_does_not_move_when_the_allocation_moves` does arithmetic on a local array and calls no module code.
- `a_store_in_the_old_format_reads_as_empty_rather_than_failing` asserts `toml::from_str` fails, not that `timings_load` returns empty.
- `test_scratch::a_directory_is_cleared_of_a_previous_run` re-implements the clearing instead of exercising `scratch`.
- `cpu_topology::the_default_is_always_at_least_one` cannot fail because of `.max(1)`.

## VAL-022 - Tests exercise compositions production doesn't use

Reported by: check-core.

- `parse_clippy_from_json`/`gated_diags` (`#[cfg(test)]`) apply only the sited filter; production applies `keep = !is_dependency_warning && !sited_allowed` inside `run_clippy_phase`. The `allow_exact` tests describe a pipeline that never runs.
- `merge_clippy_dedups_and_combines_sweeps` feeds `merge_clippy` from the text parser; production feeds JSON.

## VAL-023 - Environment-dependent check/test tests

Reported by: check-core, test-execution.

Watchdog tests need Linux `/proc` and a real process tree; `begin_phase_sets_both_bookkeepings` mutates the global `PHASE_CLOCK` and leaves it set; `everything_is_displayed_without_git` relies on `/nonexistent` not existing; three `test_runner` watchdog tests spawn `sh -c "sleep 60 & wait"`, and only one test there carries `#[cfg(target_os = "linux")]` (it needs no Linux API). See PLT-039 for the repo-wide pattern.

## VAL-024 - Stale test names and merged doc blocks in test execution

Reported by: test-execution.

`count_listed_tests_counts_only_tests` returns names; scratch name `"watchdog_attribution_never_kills"` belongs to a test asserting the opposite; `an_unknown_flag_is_left_for_the_per_binary_command` refers to per-binary commands that no longer exist. Merged doc blocks on `a_lost_start_record_does_not_escape_the_per_test_cap` ("THE CONTRACT" belongs to another test), `Ceilings`/`WallShape`, `test_argv` (stale `--list`/`Ok(0)` paragraph), `resolve_sweeps`, `unify_workspace`.

## VAL-025 - Stale claims in check/test code and `check.md`

Reported by: check-core, test-execution.

Check core:
- `CheckEntry.harness`/`Harness::Nextest` docs say the engine runs "under the project's own `.config/nextest.toml`" and "owns build, list and run"; check.md says the file is never opened and brokkr owns compile shape (config-cli-bootstrap reported the same).
- `Certifies` doc: "until coverage accounting exists".
- `SkipSpec` doc and `ResolvedSweep.qualified_skips` ("only on process-isolated sweeps, enforced at resolve time") ignore nextest and the ad-hoc path.
- `ResolvedSweep.parallel_budget` "enforced at resolve time" — actually `reject_conflicting_lanes` at run time.
- `ResolvedSweep.build_packages` says "`cargo build --release`"; the pre-build uses the sweep profile.
- cargo_filter module doc says the text path is the "rare build-error case"; it runs on every green test sweep (`filter_clippy_in_tree`), pre-builds and `brokkr test`; `ClippyParse` "errors-first" holds only for text.
- `decide_active_sweeps` doc ("Err only when --profile doesn't resolve").
- `profile.rs` documents `test_threads >= 2`/`0` "bypass the watchdog for a whole-sweep timeout", contradicting "every test gets 20s" (config-cli-bootstrap).

Test execution:
- `test_runner.rs` header: "watching libtest's partial `test name ... ` progress marker" (deleted); `LibtestRun.completed` doc describes the deferred-observe state machine and `--test-threads=1`.
- `split_trailing_event`: events "no longer decide when anything is killed" (the watchdog kills on them); `Ceilings.per_test` "Advisory" but terminates; `run_libtest_parallel` "honours a cooperative `brokkr kill` / Ctrl-C" (BUG-016).
- `check_cmd/output.rs`: "parallel ⇒ no watchdog, one whole-sweep timeout instead"; `build_test_env`: check "always tests in the dev profile".
- `parallel.rs` header "Slices are proportional to test count" (duration-weighted now); "the per-binary runs below re-enter cargo"; the `partition_target_selectors` rationale describes the old re-entry design; "sequential parallel path"; `binaries.rs`' `-Zfeature-unification` hint says "per-binary runs".
- `nextest.rs` "NOT YET WIRED" with `#[allow(dead_code)]` on items `nextest_lane` uses; measurements recorded against "cargo-nextest 0.9.143" while the engine is nextest-runner 0.124.
- `isolate.rs`: "replicating [cargo's env] is nextest's whole job, not brokkr's" — `direct_runtime.rs` now does it.
- `Ceilings::one_test` on the isolated lane (and check.md "process is the unit of exactly one test"): `cargo test <selection> -- --exact name` spawns every selected binary and the plan merges names across binaries; the 20s wall covers cargo startup plus N launches.
- `test_scratch.rs` calls itself "the one scratch-directory allocator" and cites "Eleven modules" (see PLT-038, PLT-041); its uniqueness guard is process-local (no-op under process-per-test or across the two bin crates).
- `--timeout` help: "Exact test name to run (substring filter…)".

`check.md`:
- "a tenth phase, `coverage`" (predates rustdoc and install_feature); the opening phase list omits rustdoc and install_feature.
- Rustdoc section "capped … like clippy's" vs "nothing is capped".
- "must not set `--format` … on a parallel lane" — applies to every lane; "Serial vs parallel" says the serial lane "attribut[es] a stall … from libtest's sequential output"; repeats the stale selector rationale.
- Says whole-sweep ceilings (30 min) back up and stop the run; unreachable inside `check` (VAL-010).

## VAL-026 - Misattached doc comments and stale naming in check core

Reported by: check-core.

Rendered in rustdoc (`document_private_items = true`): `reject_scoped_complete`'s paragraph on `run_sequential_resolutions`; `decide_active_sweeps`' doc on `CLIPPY_ENV_CONFLICT_REMEDY`; `describe_sweep`'s doc on `cli_package_scope`; `announce_allows`' first paragraph on `announce_test_allows`; `isolated_target_dir` and `composed_rustflags_env` each fuse two docs; a leftover comment in `run_test_phase` mentions a removed `whole_workspace` binding. Several comments still say `[clippy] allow`/`[clippy] allow_exact` (`BuildPhaseArgs`, `clippy_args` doc, `sited_match`, `gated_diags`).

Enforcement proposed: textlint (`region = "comment"`, pattern `\[clippy\] allow`).

## VAL-027 - Check/test messages name the wrong thing or say nothing

Reported by: check-core, test-execution.

- `format_hung_test` always says "killed cargo process group", even when the leader is a directly executed test binary; the serial lane's "a test exceeded its {ceiling}s budget" prints 300s for an idle kill; the nextest zero-work error says "cargo test: zero tests ran".
- `brokkr test` hand-rolls `println!("[test]    …")` ~15 times and writes stdout/stderr directly in `flush_sink` and forwarders (no `test_msg`; bypasses quiet and run log). The nextest engine prints straight to the terminal (`ReporterOutput::Terminal`) on green runs, breaking "one grouped line per phase on green". See PLT-004.
- Pluralisation is ad hoc: `format_clippy_multi` spells `error`/`errors`, `dead filter{}` pluralises manually, `filter_clippy_in_tree` prints `1 errors` (pinned by a test).
- "skipped" goes to stdout when caused by `-p`, to the log when caused by config — deliberate and documented, but each site re-implements it.
- Silent where something significant happens: watchdog thread spawn failure (`.ok()`, no ceilings), run log open failure, `PHASE_CLOCK` poisoned (ceiling skipped), the `BuildConfig` sink chosen because of an unmodelled env rustflags source.
- The verdict line lists every `[lints] allow` entry unbounded (the `allow_exact` list was collapsed for this reason); `invocation:` echoes all forwarded args.
- `DevError::Verify` is used for test timeouts.
- `timings_record` swallows every failure with nothing logged (intended).

## VAL-028 - Dead or unreachable code in the check/test pipeline

Reported by: check-core, test-execution.

- `profile::resolve_unification` returns `Result` with no `Err` path left (its docs say the refusal was removed).
- `ProfileDef.description` `#[allow(dead_code)]` for a "future `brokkr profiles`".
- `ClippyDiagnostic.is_error` constant `true` on the JSON path; `is_foreign_manifest_warning`'s `is_error` guard matters only for text.
- `ResolvedSweep::libtest_argv` test-only, doc ("Wall-clock-ordered") describes nothing.
- `NextestPair` defined, never constructed; `Disposition::is_terminal` test-only; stale `#[allow(dead_code)]` on `Disposition`/`nextest_disposition` (used).
- `run_parallel_sweep`'s `doctests` parameter (`let _ = doctests;`); `test_runner::effective_test_threads` alias of `effective_test_threads_from`; `is_bare_status_line` (marker-machine leftover used only by `test_cmd`'s display filter); `binary_selector` feeds only the reproduction line.
- `PARALLEL_SWEEP_TIMEOUT` and the parallel `timed_out` paths are unreachable inside `check` (VAL-010).
- Not dead: the "legacy fallback" branch (brokkr itself runs on it).

## VAL-029 - "Which extensions count as docs" has two owners

Reported by: convention-engines, check-core.

Gremlins scans `rs/toml/md/js/sh` case-sensitively (`SCANNED_EXTENSIONS`); `scope::PROSE_EXTENSIONS` is `md/markdown` case-insensitive. `.markdown` and `README.MD` count as prose for the markdown-only shortcut (which runs gremlins), but gremlins never scans them. The gremlins list is also written out in CLAUDE.md and check.md. `PROSE_EXTENSIONS` treats any `.md` as inert (BUG-010). Gremlins doesn't scan the repo's own `scripts/*.py`; a project can't add `.py`/`.yml`.

Enforcement proposed: one shared case-folded table with a test that every prose extension is scannable.

## VAL-030 - Dependency vocabulary is interpreted separately in each deps phase

Reported by: convention-engines.

- Dependency kind (`None` = normal, `"dev"`, `"build"`): `dependency_rules` (`DependencyKind::from`, `parse_config_kind`, `as_str` — three spellings in one file), `publish_cycle` (`KIND_*` constants and its own match), `duplicate_version`/`focus` (`kind.is_none()`), `native_code` (`== Some("build")`).
- Dependency-table names: `manifest::is_dependency_table_name` has 3; `workspace_dep::collect_from` has 5 including `dev_dependencies`/`build_dependencies` aliases, so `sort_dependencies`, `declared_deps` and `version_align` ignore alias tables `workspace_dep` reads.
- Two TOML parsers (`toml_edit`, `toml`) read the same manifests.

Enforcement proposed: a serde enum on a shared metadata type; one table-name constant; one manifest reader. See PLT-015 for the metadata deserializers.

## VAL-031 - Dependency-graph traversal is written twice

Reported by: convention-engines.

"Reverse Normal-edge adjacency plus BFS to the workspace" in `duplicate_version::run`/`via_workspace` and `focus::emit_traces`/`chains_to_workspace`, kept in step by a comment in `focus.rs`. `workspace_set` construction repeats in six phase files. Enforcement proposed: methods on `CargoMetadata`.

## VAL-032 - `TEXTLINT_PRESET_FIELDS` mirrors `TextlintRule` by hand

Reported by: convention-engines.

A hand-written list of 16 names in `parser.rs` mirroring `TextlintRule` minus 3; no test compares them, so a new field left off is silently rejected in presets. Enforcement proposed: a parity test or a `Preset` struct derived from the same field set.

## VAL-033 - `brokkr deps` bookkeeping is spelled beside the phases

Reported by: convention-engines.

`deps::run` has a literal `phases_run` list of 8 names and a hand-written `findings` sum; `deps.md`'s "adding a phase" recipe mentions neither. `phases_run` includes outdated and stale even when ccu was skipped ("ran 8 phases" false then). `ccu::PHASES[1]` is never read. `STALE_DAYS`/`ABANDONED_DAYS` are restated as "~8 months / ~2 years" in the `StaleEvent` doc and `deps.md` (approximate; no divergence). Enforcement proposed: build both from a phase table.

## VAL-034 - The displayed cargo argv is a second copy of the real one

Reported by: convention-engines.

`phase.rs` prints `cargo_line(... "cargo metadata --format-version 1 --no-deps (dependency rules)")` as a literal separate from the argv passed; they agree today. Enforcement proposed: render from the argv slice.

## VAL-035 - Convention-engine config is validated when the phase runs, not at load

Reported by: convention-engines, config-cli-bootstrap, check-core.

Textlint regexes, `region`, `join_wrapped_use` conflicts and zero-line windows are checked only in `textlint::compile`; dependency_rule `kinds` ("Validated when the phase runs"); `version_align.granularity` (inside `manifest::scan`); every header/manifest glob. A typo surfaces mid-run after earlier phases, never if the phase is skipped by `skip_phases` or the markdown-only shortcut, and under `--textlint NAME` only selected rules compile. `region`, `kinds`, `granularity` are `String`s while `MatchMode`/`Stream`/`Stage` are serde enums. `brokkr.toml.md` claims the `join_wrapped_use`+`except` error is "rejected at load time" (false).

Enforcement proposed: serde enums and compiling every rule in the parser.

## VAL-036 - Convention-engine tunables have no config surface or injection point

Reported by: convention-engines.

`DISPLAY_CAP = 200` in textlint (truncates silently, no ellipsis), `TOOLCHAIN_CRATES`, `STALE_DAYS`, `ABANDONED_DAYS`, `SUPPORTED_SCHEMA`, gremlins' `SCANNED_EXTENSIONS`; `[deps]` has one knob; nothing lists the set. `ccu::now_julian_day` reads `SystemTime` and `try_run` spawns `ccu` directly, so dedup and `OutdatedComplete` are untested and a pre-epoch clock becomes day 1970 with negative ages. `focus::tildify` reads `HOME` directly. `ccu`'s `INSTALL_HINT` hard-codes `~/Programs/check-updates/ccu` and the module doc cites the same path.

## VAL-037 - Convention-engine output is inconsistent

Reported by: convention-engines.

- `deps` JSON and `focus` JSON `println!` directly (defensible for NDJSON; no rule marks where allowed).
- Focus JSON drops `prefix_note` ("not in host-filtered graph", "substring matches"); a consumer can't tell a fallback result from an exact one.
- The `ccu` failure names only `outdated` (`PHASES[0]`) though it also skips `stale`; the renderer says nothing about stale.
- Textlint, dependency rules and script-check report counts "so ok is falsifiable"; header, manifest and gremlins print a bare `ok`.
- The gremlins failure hint "rerun with `--fix-gremlins` to rewrite all banned chars" is false for config-`ban`ned codepoints (scan-only).

## VAL-038 - Convention-engine and deps errors misclassified

Reported by: convention-engines.

`From<serde_json::Error>` gives `DevError::Build("json: ...")`, so a metadata schema mismatch reads "build: json: missing field … line 1 column N" with no phase or command; `cargo metadata` failures use `Build` not `Subprocess` (exit code lost); `brokkr deps nosuchcrate` prints "build: no package matching ...". See PLT-007.

## VAL-039 - Convention-engine tests that prove little

Reported by: convention-engines.

`header::current_year_is_sane` reads the wall clock and asserts `2024..2100`; `header::scan` has no test. No test covers the git-backed scan entry points (gremlins, header, textlint, manifest `scan`), so globs/`exclude`/walk wiring is untested. Nothing asserts every `GREMLINS` entry has a `replacement()` arm (they match today). No `TEXTLINT_PRESET_FIELDS` parity test (VAL-032). `gates_or_together_any_hit_suppresses` and check.md ("Multiple gates AND together") describe the same behaviour with opposite words.

## VAL-040 - Convention-engine guards that fail open

Reported by: convention-engines.

- `globs::build_set` uses `Glob::new` (default `literal_separator(false)`), so `*` matches `/`: `crates/*/src/**` (the preset example in `brokkr.toml.md`) also matches deeper paths. One-line `GlobBuilder` fix in the single owner.
- `manifest.adapter_group`: a `marker` matching no comment group silently no-ops; it compares dependency keys, so `foo = { package = "adapter" }` passes (dependency_rules guards this rename); `forbidden_in` names are never checked against real packages (dependency_rules errors on unknown `from`) — two implementations of one rule with opposite validation.
- `version_align`: `find_dep_version` finds no version for `{ workspace = true }`, so the check does nothing in inheriting members.
- Entries that match nothing are silent: gremlins `exclude`, textlint `exclude`, header `exempt`, manifest `exclude`/`shape_exclude`, `workspace_dep_ignore`, dependency_rule `except` — inconsistent with the parser's "dead preset config is an error" rule.
- Textlint's file count aggregates across rules, so one rule's dead `paths` is hidden whenever another matches (docs promise "a shrinking count" gives it away); header and manifest have no count, so a `[header].paths` matching nothing is green.
- `gremlins::tracked_files` drops non-UTF-8 paths silently; `scope::classify_status` treats the same case as "cannot vouch" (fail-closed).

## VAL-041 - Stale claims in convention-engine and deps code/docs

Reported by: convention-engines.

- `script_check.rs` module doc: "The child is given `BROKKR_CARGO=1`" (removed; `run_one` passes empty env); lists a nonexistent "style" phase.
- `textlint.rs` header: "Four bounded capabilities… the *only* two predicates: no arbitrary multiline" (now also `region`, `join_wrapped_use`, `skip_after`, `only_if_file_matches(_above)`, four context windows); CLAUDE.md's textlint line lists only the old four.
- `lex.rs`: "(and, later, logical-line joins)" — joins exist.
- `deps/mod.rs`: "v1 phases: `duplicate_version`" (8 phases); `deps` CLI help "v1 ships `duplicate_version`" (config-cli-bootstrap).
- `deps.md`: "serde-tagged like `CheckEvent` in `src/cargo_json.rs`" (removed); "Shells out to `cargo metadata` once per run" (twice plus `rustc -vV`); "Dispatch lands in `src/main.rs`" (in `main_parts/bootstrap.rs`); `OutdatedComplete` "ccu emits" (brokkr emits); summary lists "the phases that ran" (VAL-033).
- `check.md` manifest: "today `sort_dependencies`" (about 10 checks).
- `DependencyRule` doc: "a direct Cargo dependency that must not exist" (predates allow polarity).
- `header.rs`/docs call it a "required file header"; `scan` accepts the text anywhere (`contains`).

## VAL-042 - Convention-engine policy invented per site

Reported by: convention-engines.

- Four path-pattern dialects in one scope: globset (header, textlint, manifest), directory prefix (gremlins `exclude`), trailing-`*` prefix (`workspace_dep_ignore`), literal `"*"` (`dependency_rule.from`).
- Textlint `skip_after` and `allow_marker` each have per-line and join-pass implementations (`skip_after_suppresses`, `use_marker_suppresses`), kept aligned by comments.
- `except_above`/`require_above` (and the `_below` pair) behave identically; the docs say the names differ only to show intent; each is documented four times in `schema.rs`.
- `script_check` holds child stdout/stderr fully in memory (unbounded).

## VAL-043 - Dead code in convention engines and deps

Reported by: convention-engines.

`ccu::PHASES[1]` never read; the `(None, None) => false` arm in `dependency_rules` is unreachable (parser requires exactly one of `forbid`/`allow`; an `enum Polarity` would make it unrepresentable); `DependencyKind::Unknown` is silently skipped whenever `kinds` is set; `globs::matches` is a one-line pass-through; `workspace_root`'s `#[serde(default)]` + `is_empty` early return is a no-op path for a field cargo always emits and would silently disable the phase if it fired.
