# Bugs

Defects surfaced by the hygiene hunt (eleven read-only scopes). Hygiene findings live in `hygiene-validation.md` (VAL), `hygiene-platform.md` (PLT), `hygiene-measurement.md` (MEA) and `hygiene-projects.md` (PRJ). Nothing here has been verified; each entry records what the reporting hunter(s) read in the code, and some are explicitly marked "likely" or "inferred" by them.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## BUG-001 - `check` swallows the cause of most phase failures

Reported by: check-core.

`finish_check`'s `Err(_)` arm prints only `check failed in …` and returns `ExitCode(1)`; `main` exits silently on `ExitCode` (`src/main_parts/bootstrap.rs`). The "failing phase printed its own detail" convention holds only where a site remembered: the test phase re-prints non-`"tests failed"` errors; coverage and `verify_doc_only_rules` print explicitly. Dropped: both errors in `run_per_build_shape` ("nothing reached {phase}…", "-p …: every sweep's config rules the selection out"), `project_info` failures, spawn failures, and IO/metadata errors from gremlins, header, textlint, manifest, dependency-rules and publish-cycle. A `[[dependency_rule]].kinds` typo (validated at phase time) may surface as a bare `check failed` (unverified by the hunter).

Enforcement proposed: a `DevError::Reported` variant for "detail already printed"; the summary prints every other variant.

## BUG-002 - `check` maps a graceful `brokkr kill` to exit 1

Reported by: check-core.

`DevError::Interrupted` goes through `finish_check`'s `Err(_)` → `ExitCode(1)`, so `check` never takes main's exit-130 path or its scratch cleanup, contrary to the documented graceful-kill contract. `cmd_clippy` passes `Interrupted` through correctly.

## BUG-003 - `brokkr clippy` hides non-lint errors behind "clippy failed"

Reported by: check-core.

`cmd_clippy` treats any `DevError::Build` as an already-rendered lint failure, but `build::project_info` returns `Build("cargo metadata failed: …")` and `Build("no Cargo.toml …")`. In a non-Rust directory it prints `clippy failed in 0.0s` with no cause. `docs/commands/clippy.md` claims otherwise.

## BUG-004 - `brokkr clippy --sweep NAME` does not replay the sweep's shape

Reported by: check-core.

`build_clippy_sweep` → `sweep_from_check_entry` leaves `effective_unification = Ambient` and never calls `resolve_unification`. A `feature_unification = "package"` entry is linted as one ambient invocation; a promoted `auto` parallel sweep is not pinned to workspace. The `clippy_args` comment names this defect; `clippy.md` promises "the precise configuration a check sweep lints under". Profile `env` is also absent (clippy.md admits that part).

## BUG-005 - Pre-builds omit the unification pin (and, in `brokkr test`, `[lints] allow`)

Reported by: check-core, test-execution.

- `check_cmd/output.rs::run_sweep_pre_build` emits profile, features and `--package` but not `sweep.unification_args()`, although `ResolvedSweep::unification_args` says every invocation including the pre-build must carry it. `build_packages` binaries are built under a different feature graph from the tests that spawn them.
- `test_cmd::run_pre_build` builds its own argv without `allow_args` or `unification_args()`; check's own comment says a pre-build without allows "fails before `cargo test` is ever reached". A package-mode sweep's pre-build compiles under ambient resolution inside `unify-package`.
- The test `a_pinned_unification_reaches_every_compiling_path` checks three of six argv builders and skips both of these (and `doc_args`).

## BUG-006 - False "stale entry" warnings from `report_stale_sited_allows`

Reported by: check-core.

- `cmd_clippy` passes `packages: &[]` even when ad-hoc `-p` narrowed the run, so `brokkr clippy -p one` flags every `allow_exact` entry sited in other crates; `--lib` does the same for test-only sites.
- Only clippy results are checked, so an `allow_exact = "rustdoc::…@file"` entry warns "suppressed nothing" on every run.

## BUG-007 - Ad-hoc `--features` silently drops package-qualified skips

Reported by: check-core.

`profile::run_shaping` → `RunShaping::apply` bypasses the qualified-skip validation in `resolve_single`. A profile whose qualified skips rely on nextest entries hands them to an ad-hoc libtest shared-process sweep, where `qualified_skips` is never read, so excluded tests run.

## BUG-008 - Operator hint points at the removed `--raw` flag

Reported by: check-core, test-execution.

`cargo_filter.rs::format_test_failures` prints "rerun with --raw for the unfiltered output"; `--raw` was removed in c9b2776. A `test_runner` drain comment also still mentions `--raw`.

## BUG-009 - Sweep `env` can make the `[lints] allow` injection inert; env precedence differs by phase

Reported by: check-core.

- `rustflags::sink` inspects only the process environment. A `[[check]]`/profile `env` setting `RUSTFLAGS`/`CARGO_ENCODED_RUSTFLAGS` is refused at parse only alongside `rustflags`; otherwise cargo reads source 1/2 while brokkr injects `--config`, which is then inert.
- The test phase (`merged_env`: sweep first, project appended if absent) lets the sweep win; clippy and coverage append the brokkr pair after the sweep env, so brokkr wins (assuming last-set-wins in `run_captured_with_env`, not read by the hunter).
- All three rustflags-reading sites treat empty/whitespace `RUSTFLAGS` as unset; the hunter believes cargo treats set-but-empty as source 2, in which case `RUSTFLAGS=""` silently disables every `--config` injection and `sink_may_be_inert` stays silent. See also VAL-001.

## BUG-010 - The prose-only shortcut's premise is false for include_str'd docs

Reported by: check-core; the same premise in `git::check_clean`'s comment was reported by measurement-storage.

The shortcut rests on "documentation cannot change how the code builds". brokkr `include_str!`s `docs/**.md` into the binary (`brokkr man`), and any crate with `#![doc = include_str!("README.md")]` has doctests and rustdoc output in markdown, so a docs-only edit skips clippy, rustdoc and the tests that read those files. The `--json` trailer has no field for the shortcut: a markdown-only run reports `verdict: "passed"`. `git::check_clean` excludes `*.md` and `brokkr.toml` on the claim they "don't change what the built binary does"; `brokkr.toml` also carries host features and `capture_env`.

## BUG-011 - Watchdog exit bypasses history, cleanup and toolchain restore; `brokkr clippy` has no ceiling

Reported by: check-core, process-control, config-cli-bootstrap.

`watchdog::fire()` calls `process::exit(124)` from the watchdog thread: `history.db` never records the run (so exactly the runaway runs are missing from `brokkr history`), `RunScope` never drops, the status line is not cleared, and `LockInner::drop` never runs, leaving a `disable_toolchain` pin moved aside until the next run in that directory. `fire` also SIGKILLs descendants by bare PID without the starttime check `stray::kill` applies. `brokkr clippy` arms no ceiling at all, although the ceilings exist because of an observed 1h15m clippy hang.

## BUG-012 - Post-test script checks run on runs whose tests never ran

Reported by: check-core.

`finish_build_phases` runs `PostTest` script checks whenever `test_failure` is `None`, which includes `skip_phases = ["test"]` and the prose-only run; the comment says "only past a green test phase".

## BUG-013 - The nextest lane never passes sweep env to test processes

Reported by: test-execution.

In `check_cmd/nextest_lane.rs`, `env_refs` (sweep `env`, `BROKKR_TEST_BIN_DIR`, nidhogg's `CARGO_TARGET_TMPDIR`) reaches only `cargo metadata` and the `--no-run` build. Tests run with brokkr's environment plus cargo-config `[env]`, and brokkr's only `[env]` override (`hold::cargo_config_overrides`) carries the capability alone. Contradicts `check.md` "Env vars exported to `cargo test`"; a profile's `env = { BROKKR_TEST_PLATFORM = "1" }` is a no-op on this lane.

## BUG-014 - The parallel lane treats every non-`--test NAME` selector as a target name

Reported by: test-execution.

`run_parallel_sweep` pushes every selector from `partition_target_selectors` (`--lib`, `--bin x`, `--test=cli`, `--tests`) into `target_filters`; `filter_binaries` drops only the bare `--test` token. `brokkr check -- --lib` or `-- --test=cli` on a parallel sweep ends in "matched no test binaries", contrary to the docs.

## BUG-015 - Likely: the parallel lane counts ignored tests as passed

Reported by: test-execution (marked "likely; verify with an all-`#[ignore]` binary").

libtest emits `started` for ignored tests, so they land in `TestTracker.completed` and `passed`. If so, the `passed == 0` check added in a22fa63 can never fire and the pass count is inflated.

## BUG-016 - Ctrl-C or `brokkr kill` orphans running tests

Reported by: test-execution.

Test children are spawned with `process_group(0)`; no `SigtermGuard` is installed during `check` or `brokkr test` and no `PR_SET_PDEATHSIG` is set, so brokkr dies on the default action while cargo and test binaries keep running in their own group. The stray reaper knows only cargo-family names, so directly executed parallel-lane binaries are never reaped. The `is_shutdown_requested()` poll in `run_libtest_parallel` is dead for the same reason.

## BUG-017 - Child processes spawned with no deadline

Reported by: test-execution, convention-engines, ratatoskr, piners, pbfhogg-nidhogg, config-cli-bootstrap.

- `brokkr test`'s pre-build and `--timeout` enumeration run through `run_captured_with_env` (deadline `Duration::MAX`); `brokkr test` has no phase watchdog, recreating the build-lock park `IDLE_TIMEOUT` was written for.
- `binary_list` runs test binaries with `--list` under no deadline; its "listing executes no test code" claim ignores ctors and custom harnesses.
- `script_check::run_one` passes `Duration::MAX`: one hung script kills the whole run via the phase watchdog (exit 124) instead of failing that entry.
- `ccu` in `brokkr deps` has no timeout; network trouble hangs `deps` indefinitely.
- ratatoskr `bench_loop` calls `sidecar::run_sidecar` with no deadline and ignores the script's `ceiling:` under `--bench`; a hung harness in a gate sweep holds the global lock indefinitely.
- piners: the harness and every validator call, including network `pine-lint --tv`, run with `Duration::MAX` (lint-corpus.md claims TV "times out at 10s").
- curl in `tools.rs` (`run_curl`, `download_file`, `head_url`) has no `--max-time`, under the global lock; nidhogg `health_check` has only `--connect-timeout` (a server that accepts and hangs blocks `poll_for_ready` forever) and `curl_get_tile` has none.

## BUG-018 - Most locked commands skip the stray reap and the stale-guard warning

Reported by: process-control, pbfhogg-nidhogg.

Both run only inside `context::acquire_cmd_lock_opt`. At least 12 sites call `lockfile::acquire` directly and get neither: `BenchContext::with_build_config`, `HarnessContext::new`, `BenchHarness::new`, `VerifyHarness::new`/`pbfhogg/verify.rs`, `piners/cmd.rs`, `piners/lint/cmd.rs`, ratatoskr `service_suite`, `service_test`, `saehrimnir`, `list_smoke` (twice), `bench_gate`. CLAUDE.md and `check.md` §Strays say every locked command reaps.

Enforcement proposed: move reap and warning into `lockfile::acquire`'s fresh-hold branch and make it private to that path, or a textlint ban on `lockfile::acquire(` outside one file.

## BUG-019 - Cargo builds run without the lock

Reported by: elivagar, pbfhogg-nidhogg.

- `elivagar/dispatch.rs::run_elivagar_run` (`BuildKind::Example`) calls `cargo_build` with no lock (its own comment admits it), and `run_measured` does not take one.
- nidhogg `verify readonly` runs `cargo_build` with no lock; `bootstrap.rs` takes none for the nidhogg verify variants.
- A guarded host refuses these builds; an unguarded one builds concurrently. Enforcement proposed: `build::cargo_build` takes `&LockGuard`.

## BUG-020 - Worktree eviction and creation run before the lock

Reported by: process-control.

`context::with_worktree` calls `worktree_record::enforce` and `Worktree::create` (incl. `git worktree remove --force`) before the closure takes the lock. A second brokkr can evict or recreate a worktree the holder is building in (`is_dirty` can't see use: `target/` is gitignored). The `worktrees.toml` read-modify-write is unlocked (lost updates), and `clean --worktrees` (locked) races with it.

## BUG-021 - Worktree eviction runs on reuse and overshoots by one

Reported by: process-control.

`enforce` runs unconditionally before `create` decides whether to reuse. `docs/brokkr.toml.md` §worktree_keep and `docs/commands/clean.md` say eviction "runs when a new worktree is cut, never on reuse". Steady state is `keep + 1`; `enforce` can evict the worktree about to be reused (rebuilt cold). The `enforce` doc says it is called from `Worktree::create`; it is called from `with_worktree`.

## BUG-022 - Worktree removal has three copies; `create` force-removes without the dirty check

Reported by: process-control.

CLAUDE.md and the `remove_one` doc say `purge_all` and eviction share `remove_one`; `purge_all` inlines its own copy and `Worktree::create`'s stale branch is a third. Eviction never removes a dirty worktree; `create` force-removes a "stale" one with no dirty check, and falls back to `remove_dir_all` after discarding the `git worktree remove` error. A transient `rev-parse HEAD` failure makes `create` treat a worktree as stale.

## BUG-023 - The worktree store is keyed on the wrong root

Reported by: process-control.

`.brokkr/worktrees.toml` lives at `project_root`, but `enforce` lists worktrees per `git_root` and calls `prune_missing` against that list. Where one config directory governs several checkouts (the one-level-up layout), each run deletes the other checkouts' records, and their worktrees then sort oldest and are evicted first.

## BUG-024 - Bench stamp digests the cargo config file cargo does not read

Reported by: process-control; check-core noted the rustflags comment.

- `guard.rs::cargo_config_path`: extensionless `config` wins when both exist (matches cargo).
- `bench_cmd/stamp.rs::cargo_config_digest`/`repo_config_digests`: `config.toml` first.
- `rustflags.rs::push_config` comment says cargo "prefers `config.toml`" (both hunters believe this is backwards).
- `$CARGO_HOME` → `$HOME/.cargo` resolution is written three times. Enforcement proposed: one `cargo_config_file(dir)` plus a both-exist test.

## BUG-025 - The `bench --compare` environment gate fails open on unreadable stamps

Reported by: process-control.

`bench_cmd::check_environments` returns `Ok` when either stamp can't be read (existence was checked, so permission/IO failures pass the comparability gate).

## BUG-026 - `worktrees.toml` is hand-written unescaped and an unparseable file reads as empty

Reported by: process-control.

`worktree_record::Store::save` writes `["{name}"]` without escaping (a `"` in a name corrupts the file); `load` treats an unparseable file as empty; `save` failures are dropped (`drop(store.save(..))`). The `toml` crate is already a dependency.

## BUG-027 - Lockfile unit tests touch the real compile lock and can SIGKILL host processes

Reported by: process-control.

The `lockfile.rs` tests claim they never touch the real global lock, but `acquire_at` → `drain_and_authorize` → `drain_compile_leases` uses the real `~/.brokkr/compile.lock`, and after 20s calls `stray::reap_for_drain()`, which can SIGKILL a real rust-analyzer cargo (20s is also the per-test cap). The tests mutate process-global state without the serialising lock: `hold::publish_capability`/`clear_capability` (the hold test claims serialization the lockfile tests don't take) and `toolchain::DISABLE_DIR` (armed by a toolchain test while lockfile tests call `activate_for_lock`, a cross-module flake that can rename files in the other test's scratch). `acquire_at`, the "unit-test seam", runs the global drain, reap and capability publication.

## BUG-028 - The build script's version stamp can be stale

Reported by: process-control.

`build.rs` uses `rerun-if-changed=.git/HEAD`, which does not change when a commit moves a branch ref, and editing an unstaged file does not rerun the script; the "a `-dirty` build is never mistaken for one off a clean commit" claim is false. Its comment ".git is one level up" is wrong (sibling; code correct). It depends on `git` and `date` on PATH and ignores `SOURCE_DATE_EPOCH`.

## BUG-029 - Bench builds don't publish the child PID

Reported by: process-control.

`BenchContext::with_build_config` → `cargo_build` passes `on_spawn: None`, so `kill --hard` during a bench build cannot reach cargo; ratatoskr's build path publishes it.

## BUG-030 - `brokkr install` silently installs unlocked from a member subdirectory

Reported by: process-control.

`--locked` detection in `cmd_install` checks two guessed paths (the package dir and the `project_root` argument, actually `build_root`), not metadata's `workspace_root`; from a member subdir it installs unlocked with only an informational line. The comment says it looks "where cargo does".

## BUG-031 - `bare_run_sentinel` rewrites any `run` token followed by `--`

Reported by: process-control.

It matches the first `"run"` anywhere in argv, so a `run` that is another command's value (a mogwai target named `run`, a `--command run` value) followed by `--` is rewritten. Fix proposed: anchor on clap's subcommand position.

## BUG-032 - An empty `XDG_DATA_HOME`/`XDG_CONFIG_HOME` resolves relative to the cwd

Reported by: config-cli-bootstrap, measurement-storage.

`history.db` lands in `./brokkr/history.db`, sidecar backups in `./brokkr/sidecar-backups/` (an untracked dir that then dirties the next run's tree), and the user config is read from `./brokkr/brokkr.toml`. Per the XDG spec an empty value means unset. See PLT-003 for the resolution-rule copies.

## BUG-033 - `history --until DATE` excludes that day; `--since` accepts month 13

Reported by: config-cli-bootstrap.

String compare against `'YYYY-MM-DD HH:MM:SS'`; stored timestamps are UTC while the user date has no timezone; `validate_since` accepts month 13.

## BUG-034 - `brokkr history` never shows the ids `history <id>` needs

Reported by: config-cli-bootstrap.

`format_history` prints no id; `History.id` says ids are "shown in the leftmost column of the default view".

## BUG-035 - Routine `clean` wipes the wrong elivagar directory

Reported by: config-cli-bootstrap, elivagar.

`clean_scratch` removes `paths.scratch_dir` (default `data/scratch`) and labels it "tilegen_tmp"; the real `data/tilegen_tmp` is cleaned only if a host points scratch there, so the `dispatch.rs` dry-run comment ("a routine `brokkr clean` reclaims `tilegen_tmp`") holds only on such hosts. Nidhogg's cleanup is gated on an unrelated `scratch_dir.exists()`. See PLT-020 for clean coverage.

## BUG-036 - `DevError::Subprocess` renders spawn failures as signal deaths

Reported by: config-cli-bootstrap.

With `code: None` it prints "killed by signal"; a spawn failure also has `code: None`, so a missing `curl` renders `curl killed by signal: No such file or directory`, and a real signal death in `forward_cargo` renders "killed by signal: killed by signal 9". Needs a `Spawn` variant or `Option<signal>`.

## BUG-037 - Broken `parallel.budget = 0` error message

Reported by: config-cli-bootstrap, test-execution.

The message in `config_parts/parser.rs` (`validate_check_entry`) has about 14 embedded spaces from a lost line continuation.

## BUG-038 - Bumping `JDK_MAJOR` never re-downloads the JDK

Reported by: config-cli-bootstrap.

`tools.rs::ensure_jdk` returns if `.jdk-version` exists, regardless of its content.

## BUG-039 - The preflight hash cache can serve stale digests and loses updates

Reported by: config-cli-bootstrap.

Read-modify-write through a fixed `hash_cache.tmp` loses updates under concurrency; every tree-file miss rewrites the whole cache (quadratic); deleted files are never pruned (unbounded); the key is `path.display()` (relative and absolute duplicate; tab/newline corrupts); whole-second mtime means a same-size rewrite within a second serves a stale digest to a pinning system.

## BUG-040 - Liveness checks by bare PID treat EPERM as dead

Reported by: process-control, config-cli-bootstrap, ratatoskr, pbfhogg-nidhogg.

`clean_scratch`'s `kill(pid, 0) == -1` counts EPERM as dead (can delete a live foreign-uid process's join dir) and a recycled PID as alive forever (dir leaks). It is a bare-PID identity, which the lock code's own rule forbids. `main_parts/commands.rs` liveness check is the opposite of `ratatoskr::process::pid_is_alive`.

## BUG-041 - `BenchConfig.mode` and `brokkr_args` are never set; mode is never printed or stored

Reported by: measurement-storage.

All ~35 construction sites set `None`; `format_result_line` and `build_run_info` read `config.mode` rather than the harness's `measure_mode`, so `[result]` never prints `mode=` and `sidecar_meta.mode` is NULL for every run (the `brokkr sidecar` "mode:" line never appears).

## BUG-042 - `brokkr lock`'s "last marker" is blind for `--hotpath`/`--alloc` and sync bench

Reported by: measurement-storage.

`SidecarFifo::status_path` writes `.sidecar-status` beside the FIFO (`with_file_name`); `run_hotpath_capture` gets `paths.scratch_dir`. The reader looks at `project_root/.brokkr/.sidecar-status`. The ratatoskr bench gate has the same mismatch. It fails silently.

## BUG-043 - kv key collisions resolve the opposite way to the comment

Reported by: measurement-storage.

`build_row` orders metadata, env, `prev.*`, stderr counters; `insert_kv_row` uses `INSERT OR IGNORE`, so the first wins. The comment "runtime counters win on the unlikely key collision" is false.

## BUG-044 - A fresh `results.db` lacks `idx_runs_uuid`

Reported by: measurement-storage.

The index is created only in `migrate_uuid`; `run_migrations` returns early on a fresh DB and `SCHEMA` never creates it.

## BUG-045 - A failed results insert loses the timing and the sidecar trajectory

Reported by: measurement-storage.

`record_result` inserts before printing `[result]`; on insert failure the timing is never printed and `store_sidecar` never runs.

## BUG-046 - `--hotpath`/`--alloc` drop sidecar data on failure or kill

Reported by: measurement-storage.

`run_hotpath_capture` returns `Err` and drops its data; `run_hotpath` discards collected runs when `f(i)?` fails. `run_external_*` keeps it under `dirty`. `measure.md` claims all modes keep it.

## BUG-047 - Drifted measurement-loop copies record wrong provenance

Reported by: measurement-storage, ratatoskr, piners.

- `sync --bench` (`ratatoskr_sync/bench_gate.rs`) hard-codes `cargo_profile: Release` (and `CargoProfile` has no dev variant) although ratatoskr's config sets `[ratatoskr.harness] debug = true`, so every stored ratatoskr sync row is mislabelled; `cargo_features: None` is hard-coded and neither DB records features; `iterations: Vec::new()` for best-of-N (comment "Single measured run" is false); `measure_start_epoch` never set, so `prev.gap_seconds` includes the run's own duration (the bug `types_run.rs` documents fixing); no captured env; `run_info: None`; sidecar runs collected then dropped on failure.
- piners `measured.rs` ignores `[piners.harness] debug` (`profile_override.unwrap_or(false)`), hard-codes `cargo_profile: Release` for a dev build and patches with `meta.profile=dev`.
- See MEA-001 for the loop duplication itself.

## BUG-048 - The external timing path floors to milliseconds

Reported by: measurement-storage.

`run_external_inner` has an exact `Duration` but floors via `elapsed_to_ms` (a test asserts the truncation) and sets `elapsed_us: None`, while `us_to_ms` says rounding toward "faster than it was" is "the one error worth avoiding". Best-of-N here compares truncated ms. nidhogg `bench_api` similarly truncates curl `time_total` to whole ms (`as i64`), so sub-ms queries record 0.

## BUG-049 - `results --compare` fails to pair rows that measured the same thing

Reported by: measurement-storage, pbfhogg-nidhogg.

`brokkr_args` includes argv[0] verbatim and `normalize_brokkr_args` strips only `--commit` and `-v`, so `brokkr` vs `~/.cargo/bin/brokkr`, or `--bench 3` vs `--bench 5`, never pair. pbfhogg extract rows from `commands.rs` (`-b=<bbox>`) and `bench_extract` (`-b <bbox>`) never pair either (PRJ-004).

## BUG-050 - Baseline pins resolve by prefix, newest-wins, and can match the current run

Reported by: measurement-storage, ratatoskr.

`GateDb::lookup_baseline` matches `uuid LIKE ?1 || '%'` and takes the newest match; the current row is inserted before the lookup. A pin of `""` or a one-character prefix matching the new row makes it its own baseline (`max_delta = 0` and `equal_to_baseline` always pass); a later row sharing the pinned prefix silently becomes the baseline; `%`/`_` act as wildcards.

## BUG-051 - Ambiguous sidecar prefixes merge sessions

Reported by: measurement-storage.

`query_samples`, `query_markers` and `query_counters` use `LIKE prefix%` with no ambiguity check; `query_meta` and `query_run_info` pick an arbitrary row.

## BUG-052 - `brokkr invalidate "" -f` deletes every row; `_` is a wildcard everywhere

Reported by: measurement-storage.

No value parser on the UUID argument. `_` is a LIKE wildcard in every prefix match (uuid, commit, `command`/`mode`/`dataset` in `build_query_sql` and `query_compare`); `--grep` moved to `instr` for this reason, nothing else did.

## BUG-053 - `--meta`/`--env` can never match integer values

Reported by: measurement-storage.

They compare `value_text` only; an Int `meta.*` value never matches, silently. `--grep` coalesces all three value types.

## BUG-054 - `brokkr sidecar --where`/`--fields` fail open

Reported by: measurement-storage.

A malformed condition is ignored and every sample printed (`if let Ok(..) = parse_where_cond`); an unknown field filters everything out; an unknown `--fields` entry is dropped.

## BUG-055 - Commit identity width has no owner

Reported by: measurement-storage, process-control, elivagar.

`git rev-parse --short` length grows with the repo: a longer hash passed to `--commit`/`--compare` matches nothing; the same commit later gets a new worktree/baseline/archive name and the old one is orphaned; a user hash of a different length fails with "no build". Only the crate `build.rs` pins `--short=9`.

## BUG-056 - Sidecar marker JSONL is hand-escaped

Reported by: measurement-storage.

`sidecar_marker_json` escapes only `\\`, `"` and `\n`; a marker name with a tab or other control character (from an untrusted FIFO) produces invalid JSONL. serde_json is already used nearby.

## BUG-057 - Inferred: elivagar `bench all` fails at its tilegen arm

Reported by: elivagar (inferred, elivagar side not visible).

`src/elivagar/bench_self.rs` runs tilegen through `run_external_with_kv_raw`, which errors when stderr lacks `elapsed_ms=` (`parse_kv_stderr`); docs and `dispatch.rs` say tilegen stopped printing it at elivagar 54f9b07.

## BUG-058 - `--runs` is ignored for `pmtiles-writer`/`node-store`; `bench all` files differently shaped rows

Reported by: elivagar.

`dispatch.rs::run_elivagar_internal` passes `--runs 1`, ignores `req.runs()` while printing "N run(s)", and records no values; `bench_pmtiles.rs`/`bench_node_store.rs` pass `--runs N`, print child output and record `tiles`/`internal_runs`. The two paths file different row shapes under one command name.

## BUG-059 - `pmtiles-corpus` defaults can never succeed

Reported by: elivagar.

Every subcommand defaults to `--variant raw`, but `bless` refuses any archive that isn't locations-generated (`corpus/mod.rs::bless`); a default `check` against a locations corpus hits a contract mismatch.

## BUG-060 - Two rules locate the corpus style file

Reported by: elivagar.

`cmd.rs` defaults to `build_root/corpus/style.toml`; `corpus_style_path` uses the corpus dir's parent. With `--corpus` elsewhere, `render-manifest` records one style's hash and `check` compares another: stale forever (exit 3).

## BUG-061 - `pmtiles-corpus`/`regress` report infrastructure failures as regressions

Reported by: elivagar.

`baseline_material` covers step 1 only: in step 4 a missing `style.toml`, bad `manifest.toml` or bad `contract.json` escapes as `DevError::Io` → exit 1 ("the archive regressed"); likewise bless's `parse_baseline(...)?` and every resolve/lock/bootstrap error in `cmd::corpus`/`cmd::regress`. Corpus uses 2 for "archive refused", colliding with clap's usage error 2 (regress avoided that). The outcome→exit mapping is written in `corpus::Outcome::exit_code`, `regress::failed`/`run`, and `bootstrap.rs` (`Err(_) => 1`).

## BUG-062 - `pmtiles-stats` exits 0 on unreadable input; the PMTiles reader panics on corrupt data

Reported by: elivagar.

`pmtiles::run` prints read failures with `println!` and returns `Ok`. `read_varint` indexes out of bounds on truncated data; `decode_directory`'s `val - 1` can underflow; `vec![0; length]` allocates whatever a corrupt header says. Committed `leaves`/`digest` z/x/y and run lengths go unchecked into `xy_to_tile_id` and per-tile loops.

## BUG-063 - Tilegen "passes" with no durable archive

Reported by: elivagar.

`rename_elivagar_output` logs rename, create-dir and coinciding-dir failures but returns success; a git failure names the archive `…-unknown.pmtiles`, which the resolver never finds.

## BUG-064 - The elivagar deep clean deletes every `*.pmtiles` in the output dir

Reported by: elivagar.

`clean_elivagar_outputs` breaks the constructed-name rule its sibling documents; with `output = "data"` it would take `ocean-tiles.pmtiles`.

## BUG-065 - External-baseline benches measure or detect the wrong thing

Reported by: elivagar.

Planetiler keeps `--download` inside the timed runs (network time in the measurement); its priming check (`data_dir/sources`) likely looks in the wrong place because the child runs from `project_root`; tilemaker's `exists()` check fails on a dangling symlink; downloads get no hash check and a partial unzip counts as present; `bench_all` downgrades planetiler/tilemaker failures to "skipped".

## BUG-066 - `brokkr kill` does not stop `sync --all`

Reported by: ratatoskr.

`list_smoke.rs::run_sync_all` treats `DevError::Interrupted` as an ordinary script failure and continues; the flag stays set (guard installed once), so every remaining script spawns sæhrimnir (readiness wait never checks the flag), its harness is killed at once, and each leaves a preserved artefact dir. `service --all` stops on the first interrupt.

## BUG-067 - `brokkr kill` does not stop `sync --gate all`

Reported by: ratatoskr.

`run_gate_cohort` records `Interrupted` as a gate FAIL and continues; the next gate's sidecar `SigtermGuard::install()` resets `SHUTDOWN_REQUESTED`, so the sweep runs to completion holding the global lock. See PLT-012 for the non-reentrant guard.

## BUG-068 - The ratatoskr gate never checks build profile

Reported by: ratatoskr.

`evaluate_against_baseline` checks gate_name, script and fixture but not the stored `profile`; `--debug` runs compare against release baselines and vice versa. `GateEntry` carries `#[allow(dead_code)]` for the unread columns.

## BUG-069 - SIGTERM during sync bench orphans the mock; `sync.md` claims cleanup

Reported by: ratatoskr.

`sync.md` says mock and in-flight harness "are reaped via their `Drop` impls". With no handler installed, SIGTERM kills outright and no `Drop` runs; sæhrimnir is spawned with `isolate_pg=false` on the bench path; the graceful kill signals only brokkr's PID; the mock is orphaned with ports bound.

## BUG-070 - Sync's `run.toml` can be invalid TOML

Reported by: ratatoskr.

`list_smoke.rs::write_run_toml` builds it with `format!`, inserting paths and the features label into `"..."` unescaped. Service's `run.toml` uses `toml::to_string`; the two writers also use different schemas (`binary` vs `harness_binary`, git fields in one only).

## BUG-071 - Service script names collide with brokkr's own ratatoskr dirs

Reported by: ratatoskr.

Service artefacts go under `.brokkr/ratatoskr/<stem>`, beside `sync/`, `mock/` and `gate.db`: a `mock.lua` or `sync.lua` writes into those trees; a fixture named `readiness` becomes a dir where mock-serve does `remove_file`, which then fails.

## BUG-072 - A ratatoskr gate rule with no predicates passes

Reported by: ratatoskr.

The empty-rule-set refusal only checks `metrics` is non-empty. A metric table with no predicates, or `equal_to_baseline = false`, yields zero outcomes and the gate reports PASSED. Proposed: require at least one predicate per rule at parse.

## BUG-073 - Several pbfhogg verify checks cannot fail on content, and their FAIL lines are hidden

Reported by: pbfhogg-nidhogg.

- `check_sorted`/`compare_sort_feature` return `Result<bool>`; every caller (`verify_sort`, `verify_extract`, `verify_merge`, `verify_derive_changes`, `verify_cat`, `verify_tags_filter`, `verify_getid_removeid`, `verify_add_locations`) writes `...?;` and discards the bool.
- `verify_tags_filter` and `verify_getid_removeid` print FAIL on a diff and return `Ok` (the complement test asserts nothing); `verify_extract` never fails on a diff; `verify_add_locations`' `report_diff` always returns `Ok` and optional variants print "FAILED" and return `Ok`.
- `run_check` discards buffered detail on `Ok`, so in default quiet mode the FAIL lines are never shown and the summary says PASS.

Enforcement proposed: a `#[must_use]` verdict enum folded by `run_check`.

## BUG-074 - `VerifyHarness::diff_pbfs` ignores the exit status

Reported by: pbfhogg-nidhogg.

A crashed `pbfhogg diff` with empty stdout reads as "identical".

## BUG-075 - pbfhogg verify outputs persist between runs and pass existence checks

Reported by: pbfhogg-nidhogg.

`verify_merge` treats `diff_path.exists()` as proof the diff ran, but `target/verify/merge/` persists, so a stale OSC passes; same for `osmosis_out`/`osmconvert_out` and multi-extract strip files. `VerifyHarness::subdir` never clears anything.

## BUG-076 - `osc.rs` accepts non-OSC input as an empty diff

Reported by: pbfhogg-nidhogg.

`parse_osc_text` returns an empty diff for any non-OSC input (e.g. an empty file), and `verify_merge` then prints "element-identical PASS". Proposed: require the `<osmChange` root.

## BUG-077 - An empty variant list records nothing and exits 0

Reported by: pbfhogg-nidhogg.

`harness::run_variants` with an empty list returns `Ok`; `brokkr api --query typo --bench` records nothing and exits 0.

## BUG-078 - `verify_check_refs` with-relations mode misreads a clean dataset

Reported by: pbfhogg-nidhogg.

It lacks the `integrity_ok → 0` handling ways-only mode has, so a clean dataset reads as "could not parse counts". The parsers have no tests although their doc comments contain sample input.

## BUG-079 - `verify all` relabels real errors as SKIPPED

Reported by: pbfhogg-nidhogg.

`pbfhogg/cmd.rs::verify` resolves OSC and bbox with `.ok()`: a hash mismatch, ambiguous OSC or malformed bbox all become "SKIPPED (no --osc provided)"/"(no --bbox provided)" (the flag is actually `--osc-seq`; bbox comes from config). The suite exits 0 with 5 of 12 checks skipped. `ensure_osmosis(...).ok()` drops setup failure silently where single `verify merge` narrates it.

## BUG-080 - `suite pbfhogg` reuses a stale merged PBF

Reported by: pbfhogg-nidhogg.

`dispatch.rs::ensure_merged_pbf` keys on `{stem}-snap{key}-osc{seq}-bench-merged` to prevent "silent wrong-file reuse"; `bench_commands.rs::ensure_merged_pbf` uses `{stem}-bench-merged` and never rebuilds. The dispatch file also spells its key twice (dry-run and real).

## BUG-081 - Dataset management edits `brokkr.toml` destructively and non-atomically

Reported by: pbfhogg-nidhogg.

`download_core.rs::promote_snapshot` with `replace` deletes old snapshot files and strips their TOML blocks before checking the scratch artifact exists. Each append is read/push/write; `run_refresh` rotates the primary out then appends header, hashes, raw, builds, runs `cat`, appends indexed — a failure after the rotate leaves a dataset with no primary. Seven appenders and two line-based rewriters hand-format TOML without escaping (PRJ-009).

## BUG-082 - First-time `brokkr ingest` is impossible

Reported by: pbfhogg-nidhogg.

`nidhogg/cmd.rs::ingest` resolves the output dir via `resolve_nidhogg_data_dir`, which errors if it doesn't exist, making the `create_dir_all(data_dir)` in `nidhogg/ingest.rs` dead.

## BUG-083 - nidhogg HTTP benches and checks misjudge failures

Reported by: pbfhogg-nidhogg.

`bench_api::run_curl_timed` lacks `--fail-with-body`/`--max-time`, so a fast HTTP 500 is timed as success; `report_response_stats` defaults size and count to 0; `health_check` turns "curl not installed" into "server not running" and `serve` then reports "did not start within 6s"; `verify_geocode` reduces any curl failure to "curl request failed" and `verify_readonly` to a bare FAIL; `geocode.rs` defaults lat/lon to 0.0.

## BUG-084 - `--dry-run` for multi-extract writes files

Reported by: pbfhogg-nidhogg.

`commands.rs::build_body` for `MultiExtract` writes `multi-extract-config.json` and creates a directory; dry-run calls it, contradicting the "validate without building or running" contract. The JSON embeds `output_dir.display()` unescaped.

## BUG-085 - `nidhogg stop` signals an unverified PID and falls back to a host-wide pkill

Reported by: pbfhogg-nidhogg.

`server::is_nidhogg_process` is `cmdline.contains("nidhogg")` and guards only the SIGKILL; SIGTERM goes to an unverified PID from the pid file; `stop()` falls back to `pkill -f "nidhogg serve"` host-wide. Compare the starttime/pidfd identity rule `lockfile`/`kill` enforce.

## BUG-086 - The three indexed-PBF generators have diverged

Reported by: pbfhogg-nidhogg.

`run_refresh` passes `--type node,way,relation` to `cat`; `run` and `run_as_snapshot` don't.

## BUG-087 - `verify readonly` restore grants write it never removed and has no RAII guard

Reported by: pbfhogg-nidhogg.

Restore is `chmod -R u+w` (grants write to files that never had it); a Ctrl-C mid-test leaves the index read-only.

## BUG-088 - An unrelated `PORT` in the shell retargets nidhogg commands

Reported by: pbfhogg-nidhogg.

`nidhogg/cmd.rs::resolve_port` applies `PORT` env > `[host].port` > `DEFAULT_PORT`; `PORT` is also the contract brokkr passes into the child (`server.rs`, `bench_tiles.rs`). It re-derives the hostname and swallows hostname errors. `docs/brokkr.toml.md` never mentions `PORT`.

## BUG-089 - `corpus --bless` stamps pins and exits 0 when the harness failed

Reported by: piners.

In `piners/cmd.rs` the bless branch calls `bless::apply` and returns `Ok(())` before `run_pass` is checked; a harness exiting 1, 2 or by signal still gets surviving dispositions stamped into `pins.toml`. The exit-code sections of `corpus.md`/`lint-corpus.md` say otherwise.

## BUG-090 - `lint-corpus --bless` stamps tool failures into every pin

Reported by: piners.

`lint/cmd.rs` bless stamps `r.disposition` regardless of `tool_error`; with `pine-lint` missing every probe is `lint_error`, `--bless` writes `expected = "lint_error"` everywhere and exits 0. Lint bless also counts `!gate_ok` as "changed" and refuses no labels (corpus bless does).

## BUG-091 - A pinned break can never pass the corpus gate

Reported by: piners.

Docs say a probe can pin `expected = "compile_fail"`; the harness exits 1 on any compile/runtime break and `cmd.rs` fails the run on any non-zero exit, so a gated run containing it always fails. Lint: a pinned `piners_error`/`lint_error` passes the gate but `tool_error` still fails the run.

## BUG-092 - The corpus runtime ceiling can be silently disabled

Reported by: piners.

`CorpusDb::estimated_wall_ms` takes the newest run covering the selection with any non-null `wall_ms`, including failed runs, runs with forwarded args, or different profiles; one fast-failing `--all` run bounds every later selection at ~1s.

## BUG-093 - An older `runs.db` blocks corpus runs

Reported by: piners.

`enforce_runtime_ceiling` opens `runs.db` read-only (skipping migrations); before v4 `SELECT selector, wall_ms` fails and only `--force` gets past. `corpus-results` also only opens read-only.

## BUG-094 - A duplicate harness line aborts ingest unnamed while the gate keeps the last

Reported by: piners.

`disposition`/`trade_diff` primary keys make a repeated probe line fail with "UNIQUE constraint failed" naming no probe; `gate::evaluate` and `bless::apply` keep the last line (`BTreeMap` collect).

## BUG-095 - The lint TV anchor ignores scope

Reported by: piners.

`--reanchor` filters the fingerprint by current `--warnings`/`--all-stages`, but `lints.toml` doesn't record the scope, so a run with another scope reports false "TV advisory" divergences; `expected` has the same problem.

## BUG-096 - Registry writes are neither atomic nor locked

Reported by: piners.

`pins.toml`/`lints.toml` are written with `std::fs::write` in `bless.rs`, `reseed.rs`, `lint/cmd.rs::write_registry`, `lint/reseed.rs`; a kill mid-write truncates (an atomic helper exists: `elivagar::corpus::digest::write_atomic`). Both `--reseed` paths write without the lock `--bless` holds. `write_registry` reads the existing file with `.ok()`, so a read error drops comments.

## BUG-097 - The stored corpus verdict disagrees with the gate

Reported by: piners.

`gate::evaluate` and `ingest::insert_disposition` each compute it; a harness line for an unselected probe is ignored by the gate but stored `gate_ok = 0`, so `corpus-results` shows DEVIATES for a passing run. `gated: !args.no_gate` records `gated=yes` for bless runs where the gate was ignored (both stores).

## BUG-098 - `thead` is excluded from litehtml element scoring

Reported by: small-benches.

`is_head_path` tests `path.contains("head[")`, which matches every `thead[N]`; whole `thead` subtrees drop out. `head_paths_filtered` never tests `thead`.

## BUG-099 - Sluggrs `approve` loosens its own ratchet

Reported by: small-benches.

It copies `output.png` over `approved.png` but records the pixel diff against the old baseline, so later runs compare to the new image but are judged against the old number. A compare error maps to `0.0`. `fs::copy` happens before `set_approval`, so a DB failure leaves image and record out of step.

## BUG-100 - litehtml `approve` turns errors into perfect baselines and element errors disable the ratchet

Reported by: small-benches.

`approve` records `pixel_diff_pct.unwrap_or(0.0)` (`approve --all` does this to every errored fixture). Element-compare errors (`_ => (None, Vec::new())`) make an unreadable `pipeline.json` "no element score", so `determine_status` enforces neither threshold nor ratchet; approving stores `element_match_pct = NULL`, disabling the element ratchet permanently. Pixel-compare errors (`Err(_) => (None, Status::Error)` in litehtml `score_fixture` and sluggrs `run_snapshot`) print a bare ERROR.

## BUG-101 - Three writers of `results.db`'s `user_version`

Reported by: small-benches.

`ResultsDb` (`src/db/schema.rs`, sets 18), `MechanicalDb::migrate` (`src/litehtml/db.rs`, reads/sets 1) and `SnapshotDb` (no versioning) open the same file. Litehtml's `version < 1` migration is dead on any file `ResultsDb` touched and a future `version < 2` step would never run; a legacy v0 `runs` table opened by litehtml first is stamped 1, so `migrate_uuid` is skipped. `visibility.rs` claims they share "the file and nothing else" and that sluggrs uses `MechanicalDb` (it uses `SnapshotDb`).

## BUG-102 - `prepare.js` re-indents around `<font>` and other inline tags

Reported by: small-benches.

`INLINE_ELEMENTS` lacks `font`, `big`, `del`, `ins`, `output` (present in `compare.rs::is_inline_tag`), so `prettyPrint` treats them as blocks, the whitespace invention its comment says was fixed. See PRJ-054.

## BUG-103 - A litehtml capture/pipeline failure aborts the run and leaves a partial run row

Reported by: small-benches.

The failure propagates with `?`, leaving a `mechanical_runs` row with partial results, no summary, possibly a stale `.brokkr/capture.js`; `report` shows it as complete. Sluggrs turns the same failure into one ERROR row and continues.

## BUG-104 - Dry-run output is invisible by default for dellingr, mogwai and sluggrs hotpath

Reported by: small-benches.

`[dry-run]` lines go through quiet-gated `bench_msg` and `run_measured` sets quiet unless `--verbose`; pbfhogg/elivagar use `run_msg`. `hotpath.md` says `--dry-run` "prints them". The mogwai bare index prints its header via gated `bench_msg` and its body via raw `print!`, so without `-v` the header is missing.

## BUG-105 - Operator hints name commands that don't exist

Reported by: small-benches, piners.

"run `brokkr litehtml test` first" and "run `brokkr sluggrs test` first" (the command is `brokkr visual`); `project::require` labels ("litehtml test", "litehtml extract", "sluggrs status", …) render as `'brokkr litehtml extract' is only available…`. piners `query.rs::resolve_diff_columns` errors say `results --columns` (now `corpus-results`). Enforcement proposed: derive labels from clap, or a test that every `require` label is a `TABLE` key.

## BUG-106 - Visual commands ignore the one-level-up layout

Reported by: small-benches.

`visual`, `approve` and `git::collect` use `project_root` for build and git; the dispatcher has `build_root` and doesn't pass it.

## BUG-107 - Visual tests accept stale outputs and silently recapture deleted references

Reported by: small-benches.

`pipeline.png`, `pipeline.json`, `chrome.json` are checked with `exists()`, so a pipeline that stops writing one is compared against the previous run's file; a missing `chrome.png` is recaptured mid-run, so a deleted reference becomes a fresh one.

## BUG-108 - `visual` silently ignores flags

Reported by: small-benches.

`--suite` and `--recapture` are dropped on sluggrs (help says "litehtml only"); `--all`/`--suite` together with an ID drops the ID silently. Litehtml `expected = "fail"` fixtures that start passing report plain PASS with no "unexpected pass" signal.

## BUG-109 - `lex::use_statements` mis-parses precise-capturing bounds

Reported by: convention-engines.

Any `use` identifier starts a statement, so Rust 2024 `impl Trait + use<..>` begins a bogus statement running to the next depth-0 `;`; at top level it swallows the following real `use` and reports it at the wrong line.

## BUG-110 - `deps::human_age` prints "12mo"

Reported by: convention-engines.

`(days % 365) / 30` reaches 12, producing "12mo" or "1y12mo".

## BUG-111 - `focus::format_source` leaves non-crates.io `sparse+` registries unnormalised

Reported by: convention-engines.

`deps.md`'s source-label list omits the case.

## BUG-112 - `brokkr deps` can rewrite `Cargo.lock` or hit the network

Reported by: convention-engines.

It runs a full-resolve `cargo metadata` with no `--locked`/`--offline`; `deps.md` says it "Audits `Cargo.lock`".

## BUG-113 - `workspace_dep` swallows manifest failures and reports false findings

Reported by: convention-engines.

A root manifest read/parse failure yields an empty set and "no findings"; a member parse failure loses its inherited deps, producing false "unused" findings.

## BUG-114 - Convention engines silently skip unreadable files

Reported by: convention-engines.

`let Ok(content) = read_to_string(..) else { continue }` in gremlins scan and fix, header, textlint and manifest (manifest also skips parse failures). A non-UTF-8 file passes the header check; a Latin-1 `.md` with a raw 0xA0 (what gremlins exists to catch) is invisible. Nothing logged. Proposed: report unreadable in-scope files as violations.

## BUG-115 - pbfhogg dispatch panics if a command stops mapping through `as_pbfhogg`

Reported by: config-cli-bootstrap, pbfhogg-nidhogg.

The `unreachable!()` block in `main_parts/bootstrap.rs` covers every pbfhogg command and holds only while `as_pbfhogg()` returns `Some` for each; `MultiExtract` already moved out once. `pbfhogg/cmd.rs::verify` has `unreachable!()` for elivagar/nidhogg variants relying on caller match order; `bench_extract::strategy_args` and `bench_blob_filter::command_args` have `unreachable!()` on an unknown name. Proposed: exhaustive match over a pbfhogg sub-enum.

## BUG-116 - `report_runs` drops every other binary's report on the first error

Reported by: test-execution.

It returns at the first `run?` error, including a budget-blown one; `let _ = reporter.finish();` in the nextest lane swallows a reporter error.
