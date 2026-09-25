I found 60 findings across the eight questions, plus six bugs and likely bugs. Everything was read-only; nothing was run, built or edited. Items marked "verify" rest on behaviour of an outside tool (libtest, nextest) or a call chain I did not trace all the way. The rest were confirmed by reading the code.

## Bugs and likely bugs

1. **The nextest lane never passes sweep env to the test processes** (`src/check_cmd/nextest_lane.rs`). `env_refs` (the sweep's `env`, `BROKKR_TEST_BIN_DIR`, Nidhogg's `CARGO_TARGET_TMPDIR`) only reaches `cargo metadata` and the `--no-run` build. The engine runs tests with brokkr's own environment plus cargo-config `[env]`, and the only `[env]` override brokkr writes (`hold::cargo_config_overrides`) carries the capability and nothing else. That contradicts `docs/commands/check.md` "Env vars exported to `cargo test`", which says every test invocation gets them. A profile's `env = { BROKKR_TEST_PLATFORM = "1" }` is silently a no-op on this lane.
2. **Only `--test NAME` selectors survive the parallel lane's plan filter.** `run_parallel_sweep` adds every selector from `partition_target_selectors` (`--lib`, `--bin x`, `--test=cli`, `--tests`) into `target_filters`. `filter_binaries` (`binaries.rs`) drops only the bare `--test` token and treats every other token as an integration-target name. So `brokkr check -- --lib` or `-- --test=cli` on a parallel sweep ends in "matched no test binaries". The docs promise that selectors after `--` shape the plan.
3. **Likely: the parallel lane counts ignored tests as passed.** libtest emits `started` for ignored tests too, so they land in `TestTracker.completed` and in `passed`. If so, the `passed == 0` check added in a22fa63 ("name all-ignored parallel sweeps") can never fire, and the reported pass count is inflated. Verify with a binary whose tests are all `#[ignore]`.
4. **`brokkr test`'s pre-build is missing `[lints] allow` and the unification pin.** `test_cmd::run_pre_build` builds its own argv without `allow_args` or `unification_args()`. `check`'s `run_sweep_pre_build` passes `allow_args`, and its own comment says a pre-build without them "fails before `cargo test` is ever reached". Neither pre-build pins unification, although `sweep_selection_args`' comment calls exactly that omission a bug on the test path. So a package-mode sweep's pre-build compiles under ambient resolution inside the `unify-package` directory.
5. **Ctrl-C or `brokkr kill` orphans running tests.** Every test child is spawned with `process_group(0)`. No `SigtermGuard` is installed during `check` or `brokkr test`, and no `PR_SET_PDEATHSIG` is set. Brokkr gets the default terminate action while cargo and the test binaries, in their own group, keep running. The stray reaper only knows cargo-family names, so directly executed parallel-lane test binaries are never reaped. The `is_shutdown_requested()` poll in `run_libtest_parallel` is dead for the same reason.
6. **Several cargo and test-binary spawns have no deadline.**
   - `brokkr test`'s pre-build and its `--timeout` enumeration run through `run_captured_with_env`, whose deadline is `Duration::MAX`. `brokkr test` has no phase watchdog, so these recreate the "parked for over an hour on the build-directory lock" case `IDLE_TIMEOUT` was written for.
   - `binary_list` runs test binaries with `--list` under no deadline either. Its "listing executes no test code" claim ignores ctors and custom harnesses.

## 1. One value, one owner

- **Two 1800s wall constants:** `SWEEP_WALL_TIMEOUT` and `PARALLEL_SWEEP_TIMEOUT` are enforced two different ways (the watchdog's `wall` versus the parallel runner's `try_wait` loop). *Enforce:* one constant, one runner (see section 7).
- **The literals 20s, 280 and "five minutes" are restated by hand:**
  - 280 has no named owner at all. It lives only in the clap attribute `range(1..=280)`, the help text and the docs.
  - "20s" is hand-written in the `test_cmd` error string ("run them all at the 20s ceiling"), the CLI help and the `test_cmd` header.
  - It appears 15+ more times in `check.md`.
  - `IDLE_TIMEOUT`'s comment says it "matches check's clippy ceiling", with nothing tying it to `watchdog::phase_ceiling`.
  - *Enforce:* name `MAX_TEST_TIMEOUT` and use it in the parser. Messages can format the constants. The docs are `include_str!`'d, so a unit test can assert that `check.md` states `TEST_TIMEOUT.as_secs()`.
- **`NEXTEST_ENGINE_VERSION = "0.124.0"` is copied by hand from Cargo.toml.** The dependency is a caret requirement, so `cargo update` can move the lock to 0.124.x without the constant following. It matches today. *Enforce:* a test that parses `include_str!("../Cargo.lock")`, or an `=` pin.
- **libtest `--list` parsing exists twice:**
  - `is_list_tally` is duplicated verbatim in `test_cmd.rs` and `isolate.rs`.
  - The name extraction has already diverged: `listed_test_names` uses `trim_end` and does not deduplicate, while `parse_list_output` trims both ends, sorts and deduplicates.
  - *Enforce:* one function.
- **Two loader-path builders already disagree.**
  - `binaries::loader_path` (used by `--list`) orders libdir, then deps, then profile, then inherited, and has no build-script link-search paths.
  - `DirectRuntime::envelope` (used for execution) orders linked paths, deps, profile, libdir, inherited, and says the order "decides which same-named .so loads".
  - They also read an existing `LD_LIBRARY_PATH` differently: first match versus last match.
  - A binary that needs a build-script `.so` can therefore run but fail to list.
  - *Enforce:* listing goes through `envelope`.
- **Three owners of the cwd fallback:** `envelope` returns `"."` (and sets `CARGO_MANIFEST_DIR="."`), `run_one_binary` maps `"."` to `project_root`, and `binary_list` falls back to `project_root` itself.
- **The profile-to-subdirectory and profile-to-`--release` mapping is re-derived.** `test_cmd` uses a bool (`"debug"`/`"release"`, `!debug → --release`) in `run`, `test_argv`, `matching_test_names` and `run_pre_build`, instead of `SweepProfile::target_subdir` and `cargo_args`.
- **Pins and profile args are emitted twice.** `sweep_selection_args` already includes the profile args and `unification_args()`. The nextest build and the parallel prebuild append `unification_args()` again, and nextest appends the profile args again. It is harmless today, but the claim in `parallel.rs` that the pin comes "from that one field" is really two call sites.
- **Two owners of "does this cargo support `-Zfeature-unification`", already diverged.** `install_shape::is_toolchain_refusal` records that matching `contains("nightly")` was wrong and fixed it. `binaries::test_binaries_with_runtime` still uses `contains("feature-unification") || contains("nightly")`.
- **Package-mode builds live in two places.** The install-feature phase builds in the main target dir. A `feature_unification = "package"` test lane is isolated to `unify-package`, on the stated grounds that sharing "would thrash against every ordinary sweep". The install phase contradicts that rule, and the two package-mode builds never share a cache.
- **The same argv-parsing rule is re-implemented in four places:**
  - `narrows_selection` covers `-p`, `--package` and `--exclude`.
  - `reject_forwarded_selectors` covers the same plus `--workspace`.
  - `partition_target_selectors` and `reject_unsupported_forwarded` each re-split on `=`.
  - `--format` refusal has three policies: output.rs checks both spellings on the final argv; `direct_libtest_args` catches `--format` but not `--format=`; nextest refuses all libtest args.
  - *Enforce:* one cargo-argv classifier type.
- **`.brokkr` is joined at about 25 call sites** (test-hung, parallel-timings, nextest-synth, and others). *Enforce:* one `state_dir()` helper plus a textlint ban on `join(".brokkr")` outside it.

## 2. Values nobody can find, change, or trust

- **No tunables table.** The test-execution timing values are spread across three files: `test_runner.rs` (4 constants and `WATCHDOG_POLL`), `watchdog.rs`, and `cpu_topology.rs` (fallback of 4). `check.md` lists some of them as prose.
- **`cpu_topology` has no injection point.** `CPU_ROOT` is a hard-coded constant, so `physical_cores` is untestable, and `the_default_is_always_at_least_one` cannot fail. The count also ignores affinity and cgroup cpusets, so in a restricted container the default budget exceeds the CPUs the process may use.
- **`ParallelBinaries::resolved_budget`'s doc says `brokkr env` "reports the same figure".** `env` shows `cache_domain_cores()` without the `_or_default` fallback, so when detection fails `env` says "not detected" while the lane runs at `available_parallelism` or 4.
- **The same doc still describes the replaced claim rule** ("A binary claims `min(its test count, budget)` slots"), which `parallel.rs` records as a regression that was replaced.
- **Configuration is read late and fails open:**
  - `cargo_config_env` and `refuse_configured_runner` silently skip cargo config files that do not parse. The runner refusal fails open on a malformed file, despite its "fail closed" rationale.
  - The nextest lane's `TargetRunner::new(...).unwrap_or_else(|_| TargetRunner::empty())` silently drops a runner it cannot read.
  - Verify: `CargoConfigs::new` probably discovers config from the process cwd, not `project_root`.

## 3. One channel, one implementation

- **`brokkr test` writes around the output module.** It hand-rolls `println!("[test]    …")` about 15 times and writes `std::io::stdout/stderr` directly in `flush_sink` and the forwarders. None of this goes through `output`, so it bypasses quiet mode and the run log. There is no `test_msg`. *Enforce:* textlint `println!\("\[` outside `src/output.rs`.
- **The nextest engine prints straight to the terminal** (`ReporterOutput::Terminal`) on green runs. That breaks the "one grouped line per phase on green" policy the other lanes follow.
- **Labels are rewritten by string replacement:** `filter_clippy` output has `"cargo clippy:"` replaced with `"cargo build:"` in `test_cmd` and with `"cargo test:"` in `check_cmd/output.rs`. The label should be a parameter.
- **Messages that name the wrong thing:**
  - `format_hung_test` always says "killed cargo process group", even when the group leader is a directly executed test binary.
  - The serial lane's "a test exceeded its {ceiling}s budget" prints 300s for an idle kill.
  - The nextest zero-work error says "cargo test: zero tests ran".
- **A config error message is broken:** `config_parts/parser.rs` `parallel.budget = 0` contains 14 embedded spaces from a lost line continuation.

## 4. Errors

- **`report_runs` returns at the first `run?` error** and drops the reports of every other binary, including a budget-blown one.
- **`let _ = reporter.finish();`** in the nextest lane swallows a reporter error.
- **`timings_record` swallows every failure.** That is intended, but nothing is logged when the store cannot be written.

## 5. Tests that prove nothing

- **`test_cmd::decide_sweeps_default_profile_filters_dropped`** asserts only the label. Its comment ("Sweep struct only carries label / feature_args / build_packages") is false.
- **`binary_timings::serial_cost_does_not_move_when_the_allocation_moves`** does arithmetic on a local array and calls no module code.
- **`a_store_in_the_old_format_reads_as_empty_rather_than_failing`** asserts that `toml::from_str` fails, not that `timings_load` returns empty.
- **`test_scratch::a_directory_is_cleared_of_a_previous_run`** re-implements the clearing itself instead of exercising `scratch`.
- **`cpu_topology::the_default_is_always_at_least_one`** cannot fail because of `.max(1)`.
- **Host-dependent tests:** three `test_runner` watchdog tests spawn `sh -c "sleep 60 & wait"`. Only one test there carries `#[cfg(target_os = "linux")]`, and it needs no Linux API.
- **Stale test names:** `count_listed_tests_counts_only_tests` returns names, not a count. The scratch name `"watchdog_attribution_never_kills"` belongs to a test asserting the opposite. `an_unknown_flag_is_left_for_the_per_binary_command` refers to per-binary commands that no longer exist.
- **Merged doc blocks** on `a_lost_start_record_does_not_escape_the_per_test_cap` (the "THE CONTRACT" paragraph belongs to another test), on `Ceilings`/`WallShape`, on `test_argv` (the stale `--list`/`Ok(0)` paragraph), on `resolve_sweeps`, and on `unify_workspace`.

## 6. Guards and claims that have stopped holding

These are false today:
- `test_runner.rs` module header: "watching libtest's partial `test name ... ` progress marker". That machine is deleted.
- `LibtestRun.completed` doc describes the deferred-observe state machine and `--test-threads=1`.
- The drain comment mentions `--raw`, which was removed in c9b2776.
- `split_trailing_event` says events "no longer decide when anything is killed". The watchdog kills on them.
- `Ceilings.per_test` is called "Advisory" but terminates.
- `run_libtest_parallel` doc says it "honours a cooperative `brokkr kill` / Ctrl-C" (see bug 5).
- `check_cmd/output.rs` says "parallel ⇒ no watchdog, one whole-sweep timeout instead". `build_test_env` says check "always tests in the dev profile".
- `parallel.rs`:
  - The header says "Slices are proportional to test count"; they are duration-weighted now.
  - Lines 634–647 say "the per-binary runs below re-enter cargo".
  - The `partition_target_selectors` rationale describes the old re-entry design.
  - The "sequential parallel path" reference is stale.
  - `check.md` repeats this stale selector rationale.
  - `binaries.rs`'s `-Zfeature-unification` hint still says "per-binary runs".
- `nextest.rs` "NOT YET WIRED: no `[[check]]` entry can select the nextest harness", with `#[allow(dead_code)]` on items `nextest_lane` uses. The measurements are recorded against "cargo-nextest 0.9.143" while the linked engine is nextest-runner 0.124.
- `isolate.rs` header: "replicating [cargo's env] is nextest's whole job, not brokkr's". `direct_runtime.rs` now does exactly that.
- `Ceilings::one_test` in the isolated lane (and `check.md`'s "process is the unit of exactly one test") is not true there. `cargo test <selection> -- --exact name` spawns every selected binary, and the plan deliberately merges the same name across binaries. The 20s wall covers cargo startup plus N binary launches.
- The docs say the whole-sweep ceilings (30 min) back up and stop the run. Inside `check`, the 15-minute test-phase watchdog always fires first, so the parallel lane's `timed_out` branch and its "(1800s)" messages are unreachable.
- `check.md` "Serial vs parallel": the serial lane "attribut[es] a stall … from libtest's sequential output" is out of date. The `--format` restriction is described as parallel-only but applies to every lane.
- `test_scratch.rs` calls itself "the one scratch-directory allocator". About 20 tests use `std::env::temp_dir()` (under `/tmp`, against project rules), `preflight.rs` uses `CARGO_MANIFEST_DIR` directly, and "Eleven modules" is a count that drifts. *Enforce:* textlint forbidding `temp_dir()` in `src/**/*.rs`. The uniqueness guard is also process-local, so it is a no-op under process-per-test execution or across the two bin crates.
- `--timeout` help: "Exact test name to run (substring filter…)" contradicts itself.

This one is checkable: `enforce_single_threaded` and the serial lane's second `--test-threads=1` requirement are left over from the partial-marker machine. The JSON tracker handles concurrency, and `run_libtest_parallel` proves it.

## 7. Policy invented per call site

- **Two libtest runners** (`streaming_run_libtest` and `run_libtest_parallel`) share the tracker but differ by accident:
  - the wall clock (watchdog `wall` versus `try_wait`);
  - whether a wall kill takes a `/proc` snapshot (serial does, parallel does not);
  - cancellation (abort and shutdown checks only in parallel);
  - `build_elapsed` tracking (serial only);
  - the timeout message.
  - The right move is one runner taking `Ceilings`, an optional abort flag and a program.
- **Same condition, different handling across lanes:**
  - doctests-on-an-isolated-lane warns every run, while the parallel lane refuses at config load, and its own comment explains why a per-run warning is wrong;
  - forwarded args: refused by the isolated lane, partitioned by parallel, cargo-only for nextest;
  - target runners: parallel refuses them, nextest honours them, and silently drops one it cannot parse.
- **Two independent copies of nextest engine setup:** `run_nextest_sweep` and `nextest_shape_cases` each contain about 120 identical lines (host detection, metadata, configs, build, builder, synth config, profile, ctx, `TestList`). They have already diverged: only the audit path has the `build-finished` guard.
- **Four `/proc` walkers and three group-signal helpers:**
  - `test_runner` (`status` PPid and `task/children`), `watchdog.rs`, `stray.rs` and `ratatoskr/process.rs` each walk `/proc`;
  - group signalling appears in `test_runner`, `ratatoskr/process.rs` and `main_parts/commands.rs`;
  - `test_runner` reaches into `ratatoskr::process::snapshot_proc`, a dependency pointing the wrong way. *Enforce:* a `[[dependency_rule]]`.
- **Ambient state read from logic:** `DirectRuntime::envelope` (`std::env::var_os`, `LD_LIBRARY_PATH`), `cargo_program` (walks `PATH`), `binary_list`, `composed_rustflags_env`, `hold::cargo_config_overrides` (`HOME`), and `capture_hung_test` (`SystemTime::now`).
- **Unbounded growth:**
  - `.brokkr/test-hung/<ts>-<pid>-<name>/` snapshots are never cleaned, and `clean.md` does not mention them.
  - The parallel lane spawns one OS thread per planned binary up front, plus three more per running binary.
- **Shared fixed files:** `nextest-synth.toml` and `~/.brokkr/nextest-env.toml` are rewritten non-atomically on every run. That is safe only because of the global lock.

## 8. Code that is no longer load-bearing

- **`NextestPair`:** defined, never constructed (the only references are its own definition and doc).
- **`Disposition::is_terminal`:** used only in tests.
- **The `#[allow(dead_code)]` attributes** on `Disposition` and `nextest_disposition` are stale; `nextest_lane` uses both.
- **`run_parallel_sweep`'s `doctests` parameter:** only `let _ = doctests;`.
- **`test_runner::effective_test_threads`:** a pure alias of `effective_test_threads_from`.
- **`is_bare_status_line`:** lives in `test_runner` and is used only by `test_cmd`'s display filter, a leftover of the deleted marker machine.
- **`binary_selector`:** now only feeds the reproduction line.
- **`WallShape::OneTest` on the isolated lane:** asserts a shape that does not hold (see section 6).
- **`PARALLEL_SWEEP_TIMEOUT` and the parallel `timed_out` paths:** unreachable inside `check` (the only caller), because the 15-minute phase ceiling fires first.
- **Structural question:** `isolation = "process"` and `harness = "nextest"` are two implementations of process-per-test. The docs already call nextest the concurrent form of the same guarantee.

## What can be enforced mechanically

Everything below fits engines brokkr already has:
- **Textlint rules:**
  - `println!("[` outside `output.rs`;
  - `temp_dir()` in `src`;
  - `join(".brokkr")` outside one helper;
  - numeric second-counts in `docs/commands/check.md` that should cite the constant name.
- **Unit tests:**
  - `Cargo.lock` against `NEXTEST_ENGINE_VERSION`;
  - `check.md` against `TEST_TIMEOUT` and `MAX_TEST_TIMEOUT`;
  - listing and execution environments built by one function, asserted equal.
- **Types:**
  - one `LibtestInvocation` builder carrying profile, pin, allows and selection, so the pre-build, enumeration and run argvs can't be spelled separately;
  - one `CargoArgv` classifier for selectors and unsupported flags.
- **Dependency rule:** `test_runner` may not depend on `ratatoskr`.
- **Not mechanically enforceable:** the stale prose in section 6. The only fix there is to delete rationale that describes removed designs, so it stops being read as current.
