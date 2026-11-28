# Bugs

Defects turned up by the hygiene hunt. Each entry says what the hunter traced; none has been verified.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.

## Check and test

## BUG-001 - CARGO_PKG_VERSION_PRE is wrong when build metadata contains a dash

Sites: `src/check_cmd/direct_runtime.rs`.
The direct launch envelope splits the package version at `-` before removing `+` build metadata, so `1.2.3+linux-x86` yields `CARGO_PKG_VERSION_PRE=x86`.
Reported by: check-orchestration.

## BUG-002 - Libtest listing tally adds child-supplied counts unchecked

Sites: `src/check_cmd/isolate.rs`.
The text listing tally sums `u64`s from the child with unchecked addition: a panic under overflow checks, a wrap otherwise.
Reported by: check-orchestration.

## BUG-003 - Serial check accepts complete-looking text when the typed stream is incomplete

Sites: `src/check_cmd/test_execution.rs`, `src/test_runner/outcome.rs`, `reference/check-internals.md`.
The typed reducer can return `Incomplete`, but serial `check` handles only `Stopped` and `Fail`, then takes counts and completeness from reconstructed `running` / `test result` lines. A clean exit with complete-looking text and a stream that never reached EOF passes, contrary to the documented fail-closed invariant. Suggested fixture: valid records, then exit while a descendant holds stdout open.
Reported by: test-runner.

## BUG-004 - Focused test success compares counts, not identities

Sites: `src/test_cmd/verdict.rs`, `src/test_cmd/plan.rs`, `src/test_cmd/run.rs`.
Success compares `terminal_count()` with `want`; the matched names are used only to classify failures. A harness reporting the same number of different names passes, including under an exact `--timeout` invocation.
Reported by: test-runner.

## BUG-005 - Focused test drops the crash diagnosis when red tests are present

Sites: `src/test_cmd/verdict.rs`, `src/test_runner/outcome.rs`, `docs/commands/test.md`.
The harness-failure fallback is guarded by `!red`, and the typed exit renderer also suppresses the exit when the stream has a red test. So a signal after a failing test loses its crash report, contrary to the manual. The fallback also labels a sorted list of in-flight names as the "last test seen starting".
Reported by: test-runner.

## BUG-006 - Focused journal records BuildFailed lanes as passed

Sites: `src/test_cmd/run.rs`.
`LaneFinished.passed` is `outcome != Fail`, which admits `BuildFailed`. A failed recheck can leave lanes without a recorded build failure. The fallible `single_run` construction can exit without running the run-stop closure, and an invalid sweep index silently falls back to lane zero.
Reported by: test-runner.

## BUG-007 - Focused build-error scan misreads doc-only test output as a compiler error

Sites: `src/test_cmd/verdict.rs`.
The scan checks all Cargo stderr, so a doc-only test's `error:` line can be classified as a compiler error when lifecycle observation is absent.
Reported by: test-runner.

## BUG-008 - Harness shim verdict decoder accepts unknown status bytes

Sites: `src/test_runner/harness_shim.rs`.
Unknown status bytes are treated as `Accept` candidates if a descriptor is present, and socket EOF is reported through `last_os_error`.
Reported by: test-runner.

## BUG-009 - Doctest accounting and argv disagree about whether doctests run

Sites: `src/check_cmd/test_execution.rs`, `src/check_cmd/accounting/plan.rs`, `src/check_cmd/check_plan.rs`, `src/check_cmd/prepare.rs`.
The argv builder reads forwarded selectors, including `--doc`, but `lane_runs_doctests` reads only the sweep and the project boolean. A forwarded named test can make accounting predict doctests Cargo will omit, and forwarded `--doc` can cause the reverse. Carrier derivation uses ambient doctest facts while carrier preparation queries them under the lane env.
Reported by: check-orchestration.

## BUG-010 - Support artifacts are attributed to the requested package, not their own

Sites: `src/check_cmd/test_execution.rs`, `src/check_cmd/binaries.rs`, `src/check_cmd/direct_runtime.rs`, `src/check_cmd/prepare.rs`.
`support_artifacts` ignores the reported package id and gives every accepted executable the requested package's name. Two paths for one `CARGO_BIN_EXE_<name>` both survive and are applied in sequence. Hashing happens after builds, so a shared uplift path gives both identities the last writer's bytes. The fingerprint omits linked-library directory contents.
Reported by: check-orchestration.

## BUG-011 - Cargo build-finished means three different things

Sites: `src/check_cmd/binaries.rs`, `src/check_cmd/nextest_lane.rs`, `src/check_cmd/test_execution.rs`.
Binary discovery accepts any `build-finished` record regardless of `success`. The nextest precheck searches for a literal substring, so a non-record or nested text can satisfy it and a whitespace change can defeat it. The serial gate parses and uses `success`. Discovery also drops malformed artifact records silently, and the serial verifier accumulates strings with no bound until the terminal record.
Reported by: check-orchestration, cargo-integration.

## BUG-012 - Nextest maps an independent abort after cancellation to Interrupted

Sites: `src/check_cmd/nextest_lane.rs`, `src/check_cmd/accounting/reconcile.rs`.
Any abort received after cancellation began becomes `Interrupted`, so a sibling that aborted on its own loses its true outcome. The cancellation-to-event mapping also repeats the reconciler's.
Reported by: check-orchestration.

## BUG-013 - Background script failure can overwrite the reported failed phase

Sites: `src/check_cmd/phase.rs`, `src/check_cmd/concurrency.rs`, `src/check_cmd/verdict.rs`.
A background stage failure, or a saved Cargo env failure, can replace the summary's `failed_phase` after a timeout while the primary error describes another phase. The trailer then names the wrong scope.
Reported by: convention-gates, check-orchestration.

## Convention gates

## BUG-014 - Gremlins ban entries for LF and CR are accepted and never checked

Sites: `src/gremlins.rs`, `src/config_parts/parser.rs`.
Config load accepts `ban` entries for LF and CR, but the scanner skips both before the ban lookup.
Reported by: convention-gates.

## BUG-015 - Textlint excerpt cap drops the last character of a 200-character line

Sites: `src/textlint.rs`.
The 200-character cap clips an exactly-200-character line. The multiline excerpt applies the cap a second time over its own capped prefix.
Reported by: convention-gates.

## BUG-016 - Manifest checks inherit from whichever workspace table is scanned first

Sites: `src/manifest.rs`.
Both the adapter-group check and version alignment use the first scanned `[workspace.dependencies]`. A nested or standalone workspace scanned first supplies the wrong aliases or versions. The adapter checker resolves a member's alias against that member's own table while version alignment uses the scan-wide first table, so the same edge has two identities. Also: the adapter marker takes the first substring match, and full `[workspace.dependencies.NAME]` subtables vanish from the adapter group.
Reported by: convention-gates.

## BUG-017 - Script check passes on a sentinel followed by a crash

Sites: `src/script_check.rs`, `src/config_parts/parser.rs`, `src/fs_probe.rs`, `src/output/child.rs`.
A sentinel followed by a crashing or signalled process passes. A whitespace-only `expect` passes load and can match empty output in `exact` mode after trimming. A pipe poll or read error returns the same `None` as EOF and drain-thread join failure is discarded, so a retained sentinel looks like complete output.
Reported by: convention-gates.

## BUG-018 - Script check stream selection: an unselected stream can fail the gate

Sites: `src/script_check.rs`, `src/check_cmd/conventions.rs`.
Truncation is checked only on the selected stream, but an open-pipe drain is rejected globally because the supervisor records one `output_cut` bit. An unselected stream can fail the gate on drain grace while an overflow on it passes. The rustc-shaped failure branch returns before showing either truncation marker.
Reported by: convention-gates.

## BUG-019 - Fixture lint can certify missing artifact evidence

Sites: `src/check_cmd/fixture_lint/mod.rs`, `src/check_cmd/member_check.rs`.
`fresh` defaults to false and an absent `profile` passes. For a lib fixture any non-`bin` kind matches, including unknown kinds.
Reported by: convention-gates.

## BUG-020 - Fixture lint verdict attribution gaps

Sites: `src/check_cmd/fixture_lint/mod.rs`, `src/check_cmd/fixture_lint/manifest.rs`, `src/config_parts/check.rs`.
A warning with no package id is filtered out before the unattributed-warning check. One warning satisfies repeated identical expectations. A trailing unpaired backtick yields a token (`split` plus `step_by`). `same_lint` folds dash and underscore separately from the central lint identity.
Reported by: convention-gates.

## BUG-021 - Fixture Cargo.lock comparison treats two read failures as equal

Sites: `src/check_cmd/fixture_lint/mod.rs`.
Before/after lock reads use `.ok()`, so two failed reads compare equal. An operational error inside the variant loop skips the after-check, and a seeding failure leaves changed state behind.
Reported by: convention-gates.

## BUG-022 - Header check passes the time() failure sentinel through as a timestamp

Sites: `src/header.rs`, `src/check_cmd/conventions.rs`.
The entry point calls `libc::time` on every header run, even for a literal pattern, and `-1` reaches civil-date conversion as a 1969 timestamp.
Reported by: convention-gates, host-fs-output.

## Cargo integration and launch

## BUG-023 - Injected cfg(all()) rustflags can lose to a later -D

Sites: `src/rustflags.rs`.
The injection comment promises `cfg(all())` lands last, but reconstruction traverses matching keys in key order and `cfg(all())` sorts ahead of `cfg(target_os = ...)`, so a later `-D` can beat the injected `-A`. A plain encoding can also be promoted to encoded after the sink was announced as `RUSTFLAGS`.
Reported by: cargo-integration.

## BUG-024 - Text diagnostic parser invents lint codes from bracketed text

Sites: `src/cargo_filter.rs`, `src/cargo_json.rs`.
Any bracket pair in the tail is treated as a lint suffix, so a message containing `[...]` followed by more text gains a fake lint code and loses the rest. The linker cleanup hint reaches forward across unrelated diagnostics to the next `could not compile`.
Reported by: cargo-integration.

## BUG-025 - cargo_env help scan stops at a global option's value

Sites: `src/cargo_env.rs`.
`needs_evaluation` stops at the first plain word, including the value of a global option that `subcommand` knows to skip. A help or version launch after `-C dir` or similar can run an evaluator. Conflict discovery still walks the original cwd under `-C`.
Reported by: cargo-integration, config-cli-startup.

## BUG-026 - cargo_env evaluator sees the environment before the caller's removals

Sites: `src/output/child.rs`, `src/process/spawn.rs`, `src/cargo_env.rs`.
The unsetting runner builds the command and runs `[cargo_env]` before applying removals. A fixture variant that unsets a variable can compute a value from it and then launch Cargo without it.
Reported by: cargo-integration.

## BUG-027 - install --path package-local [env] escapes the conflict check

Sites: `src/build_info.rs`, `src/cargo_env.rs`, `src/rustflags.rs`.
Install root discovery knows Cargo reloads config from the `install --path` package, but the `[env]` conflict check starts from the launch cwd.
Reported by: cargo-integration.

## BUG-028 - Crit config digest misses include contents

Sites: `src/crit_cmd/stamp.rs`.
The config digest walk fingerprints root documents by depth and misses their includes, so an include edit can change compiler or linker behaviour without changing the stamp. A failed compiler probe is silently omitted.
Reported by: cargo-integration.

## BUG-029 - Guard install panics on a scalar build key

Sites: `src/guard.rs`.
A valid TOML document whose `build` is a scalar reaches `doc["build"][WRAPPER_KEY]`, where the locked `toml_edit` panics.
Reported by: cargo-integration, lock.

## BUG-030 - Guard remove reports success without removing

Sites: `src/guard.rs`.
An inline `build` table is recognised, but removal uses only `as_table_mut` and reports success when nothing was removed. A non-string wrapper is treated as absent and can be overwritten. Install and remove are read-modify-replace with no compare-before-commit, so an intervening edit is lost.
Reported by: lock.

## BUG-031 - Guard handshake accepts a probe that exited unsuccessfully

Sites: `src/guard.rs`.
The probe accepts a matching prefix and version on stdout without requiring a successful exit.
Reported by: lock.

## BUG-032 - Snapshot publication trusts whatever already sits at the destination

Sites: `src/runnables.rs`, `src/launch/helper.rs`.
Snapshot publication hashes the staging copy, then trusts any file already at the digest/name destination, including an edited file or a symlink. Stage names use PID plus a lossy basename. Helper reuse compares bytes but accepts a bad mode.
Reported by: launch-and-install.

## BUG-033 - Install rerun refuses at an existing stamp

Sites: `src/build_info.rs`, `src/runnables.rs`.
`stamp_file` refuses any existing stamp, and the caller doesn't distinguish a newly installed file from a Cargo run that left a stamped binary in place. A rerun reaches that refusal before post-install steps.
Reported by: launch-and-install.

## BUG-034 - build.rs treats a failed git status as a clean tree

Sites: `build.rs`, `src/build_info.rs`.
`build.rs` silently treats failed or empty porcelain output as clean. `build_info::git_line` collapses missing Git, failed commands and invalid UTF-8 into the ordinary unstamped case.
Reported by: launch-and-install.

## BUG-035 - passthrough --runs reserves capacity for any nonzero usize

Sites: `src/passthrough_cmd.rs`.
An extreme `--runs` reserves that capacity immediately and can panic or fail allocation.
Reported by: launch-and-install.

## BUG-036 - Non-UTF-8 argv or environment panics before any refusal

Sites: `src/main_parts/bootstrap.rs`, `src/bin/fstrim_helper.rs`, `src/config_parts/parser.rs` (`captured_env_map`), lock argv collectors.
Startup collects `std::env::args()` twice, including before the version fast path, and on Unix it panics on non-UTF-8 arguments. The fstrim helper repeats this. `std::env::vars()` in env capture panics on an unrelated non-UTF-8 entry before the name filter runs.
Reported by: launch-and-install, lock, process-and-reservations, config-cli-startup, host-fs-output, bench.

## BUG-037 - Data output swallows write failures, raw prints panic on broken pipes

Sites: `src/output/console.rs` (`data_stdout`), `src/passthrough_cmd.rs`, `src/man.rs`, `src/history_cmd.rs`, `src/results_cmd.rs`.
`data_stdout` discards write and flush errors, so `build` can exit successfully without delivering its path. Passthrough timing, history and results use `println!`, which can panic on a closed pipe. `man` treats a broken pipe as success and writes other failure warnings to stdout. Run-log writes discard errors, including a full disk.
Reported by: launch-and-install, results-server-and-stores, config-cli-startup, host-fs-output.

## Lock, process and reservations

## BUG-038 - An unreadable holder can be reported as released

Sites: `src/lock/mod.rs`, `src/lock_cmd.rs`.
`status()` represents held-but-unreadable as a synthetic `LockInfo` with PID zero and an empty acquisition id. `stop_unconfirmed()` treats any changed id as release, so it can report success while the flock is still held. Status can also show the synthetic identity as an unconfigured invocation.
Reported by: lock.

## BUG-039 - Same-process lock re-entry races registration

Sites: `src/lock/hold_protocol.rs`, `src/lock/mod.rs`, `src/main_parts/dispatch.rs`.
`HELD` is checked early and populated after service startup, drain, authorisation and cgroup entry. A second thread can miss it, open another descriptor, and poll forever for a flock this process owns. A later re-entrant guard can take the one-shot maintenance token before dispatch does.
Reported by: lock.

## BUG-040 - Lock kill: a queue error prevents the holder stop

Sites: `src/lock_cmd.rs`.
A directory-level queue list error returns through `?` before the holder is examined. A holder-read error can discard the accumulated cancellation report and skip serve reporting.
Reported by: lock.

## BUG-041 - Queue ticket sequence overflow and cleanup failures

Sites: `src/lock/queue.rs`.
An on-disk ticket sequence of `u64::MAX` makes `seq + 1` overflow. A successful flock followed by a failed ticket unlink can forfeit older tickets and abort the new acquisition. A comment claims kill sweeps dead waiters, but kill's path only lists.
Reported by: lock.

## BUG-042 - Hard kill task-stop poll misses traced-stopped tasks

Sites: `src/lock_cmd.rs`.
The task-stop poll treats only `T` as stopped, while holder status detects `T` and `t`. It also proceeds silently after its timeout.
Reported by: lock.

## BUG-043 - Orphan reaper unlinks every registration when /proc can't be listed

Sites: `src/test_orphans.rs`.
`read_table` returns an empty table when `/proc` can't be listed, and `reap_in` then unlinks every claimed registration as carrier-free. Unreadable environments count as unmarked. The serve reaper already distinguishes total scan failure from an empty result.
Reported by: test-runner, process-and-reservations.

## BUG-044 - Group-signal helper accepts process group 1

Sites: `src/process/helpers.rs`, `src/shutdown.rs`.
The generic group helper accepts `1`, which becomes the special `kill(-1, ...)` case. The shutdown handler separately rejects it.
Reported by: process-and-reservations.

## BUG-045 - Test leader ignores poll failure

Sites: `src/test_runner/leader.rs`.
The leader ignores the pidfd poll outcome, so repeated poll failure can busy-loop.
Reported by: process-and-reservations.

## BUG-046 - Exact spawn misreads marker bytes

Sites: `src/process/spawn.rs`.
Exact spawn treats any byte other than the refusal marker as an exec-stage marker. `read_marker` drops read errors.
Reported by: process-and-reservations.

## BUG-047 - Lease cleanup renders a store-wide error as a path glob

Sites: `src/reservations/mod.rs`.
A store-wide read error is represented by id `*` and rendered with the per-lease manual-removal template, producing `*.*` under the store, unquoted.
Reported by: process-and-reservations.

## BUG-048 - Worktree target removal trusts a suffix match

Sites: `src/worktree.rs`, `reference/clean.md`.
A reused slot recreates the destination of its dangling target link. Removal accepts any destination ending in `brokkr-worktrees/<key>/<short>`, so a repointed link with that suffix lets brokkr write to or delete a different tree. The documented ownership rule says a repointed destination is left alone.
Reported by: cargo-integration, launch-and-install, process-and-reservations, worktree-clean-git-deps.

## BUG-049 - Toolchain pin restoration has two writers with opposite rules

Sites: `src/toolchain.rs`, `src/tree_policy.rs`.
Normal `Drop` renames the saved pin over whatever is there, so a pin recreated during the hold is lost. Watchdog recovery restores only if the original is absent. A later activation with both names present deletes the sidecar as stale. Recovery depends on `disable_toolchain` still being set, unlike Cargo config recovery, so a killed run followed by removing that option leaves the pin absent. Normal cleanup restores config before the pin and the backstop does the reverse.
Reported by: convention-gates, cargo-integration, lock, config-cli-startup.

## BUG-050 - Worktree slot keyed by a 9-digit commit prefix

Sites: `src/git.rs`, `src/worktree.rs`, `src/crit_cmd/mod.rs`.
A commit-prefix collision takes the stale-replacement path, so a pristine slot can be evicted by another commit with the same prefix. Criterion can reuse the same baseline label. A transient canonicalisation failure creates a second checkout identity. Discovery accepts any nonempty hex basename.
Reported by: worktree-clean-git-deps.

## Config and CLI

## BUG-051 - Config provenance keeps a stale array origin after a table-array merge

Sites: `src/config_parts/compose.rs`.
An empty or ordinary array records a value origin, and a later table-array merge leaves that origin in place even after entries from several files contribute. Also: `surviving_user_entries` predicts the fold's result separately from `merge_entries`.
Reported by: config-cli-startup.

## BUG-052 - User presets are dropped more broadly than documented

Sites: `src/config_parts/compose.rs`, `src/config_parts/parser.rs`, `docs/brokkr.toml.md`.
`drop_orphaned_user_presets` drops any unreferenced user-only preset, including ones never referenced, and so bypasses the dead-preset and unknown-field checks. The docs describe an exception only for presets orphaned by shadowing. Making this fatal needs config-contract approval.
Reported by: config-cli-startup.

## BUG-053 - An inaccessible config can look absent

Sites: `src/project.rs`, `src/main_parts/bootstrap.rs`.
`find_config_dir` uses `Path::exists`, so a metadata failure such as a symlink loop sends discovery to the parent or to unconfigured mode, and `check` can run without its gates. Startup otherwise refuses to ignore a broken project config. Turning this into a refusal needs config-contract approval.
Reported by: config-cli-startup.

## BUG-054 - results --rename with a bad arity falls through to a query

Sites: `src/main_parts/dispatch.rs`.
The `--rename` conversion silently discards a bad arity and runs a query. `results --json UUID` ignores explicit `--top` and `--limit`.
Reported by: config-cli-startup, results-server-and-stores.

## BUG-055 - fmt and env take the lock under different conditions

Sites: `src/cli/visibility.rs`, `src/main_parts/bootstrap.rs`, `src/env.rs`, `src/main_parts/dispatch.rs`.
Both are `LockPolicy::None`. `env` acquires when a tree policy is active or strip recovery is needed, while `fmt` acquires only for `disable_toolchain`. A strip-only tree is filtered for `env` but not `fmt`, and `fmt` misses strip recovery. `install --names` queries Cargo but suppresses the configured tree policy, so it can fail on a missing pinned toolchain where `install` works.
Reported by: config-cli-startup, worktree-clean-git-deps.

## BUG-056 - Dry runs and refusals have effects

Sites: `src/main_parts/dispatch.rs`, `src/lock/hold_protocol.rs`, `src/run_log.rs`, `src/clean/layout.rs`.
`.brokkr/.gitignore` is created before `reads_only` is consulted. Retired clean flags are refused after lock acquisition, maintenance and live-tree activation. `clean --dry-run` still takes the hold, creates and prunes run logs, runs the fresh-hold reapers, and activates or recovers tree edits. It also bypasses the reservation gate, so it can predict removals that would be refused and miss lease reclamation.
Reported by: config-cli-startup, worktree-clean-git-deps.

## BUG-057 - serve --grace values that the reader can't parse

Sites: `src/cli/schema.rs`, `src/serve/registry.rs`, `src/serve.rs`.
`--grace` accepts any `u64` seconds. The intent writes `as_millis()` as a `u128` and the reader parses `u64`, so a large value survives parsing, build and publication, then leaves the supervisor unable to read its intent. `--ready ""` succeeds immediately.
Reported by: config-cli-startup, worktree-clean-git-deps.

## BUG-058 - Results-server URL refusal echoes the credentials it refuses

Sites: `src/config_parts/user.rs`, `src/bin/results_server/deploy.rs`.
URL validation refuses userinfo but puts the original URL, credentials included, into the error. Deploy prints the configured URL.
Reported by: launch-and-install, bench, results-server-and-stores, config-cli-startup, host-fs-output.

## Host and output

## BUG-059 - Mount parsing corrupts non-ASCII mount points

Sites: `src/host/storage.rs`.
Storage decodes `/proc/mounts` by turning each UTF-8 byte into a separate scalar, and accepts decimal digits in octal escapes. A failed match can pair a path's `statvfs` figures with the parent mount's label, and deduplication then skips the real mount.
Reported by: host-fs-output.

## BUG-060 - CPU inventory reports per-package cores as physical cores

Sites: `src/host/cpu.rs`, `src/env.rs`.
Inventory takes the first `cpu cores` in `/proc/cpuinfo` and labels it physical cores next to a machine-wide processor count, which is wrong on multisocket hosts. `env` reports the raw cache-domain count as the parallel budget even when execution caps it by `cpu.max`, and "not detected" when execution uses a fallback.
Reported by: host-fs-output.

## BUG-061 - Passwd lookup treats an undersized buffer as absence

Sites: `src/user_dirs.rs`.
`getpwuid_r` returning a too-small-buffer error is treated as no account, with no retry.
Reported by: host-fs-output.

## BUG-062 - Child drain idle timestamp can move backwards

Sites: `src/output/child.rs`.
Each drain thread reads elapsed time and stores it into a shared atomic. A thread delayed between the two can overwrite a later stamp, so the idle deadline measures from older activity.
Reported by: host-fs-output.

## BUG-063 - Run-log pruning orders names lexically

Sites: `src/run_log.rs`.
Filename creation parses numeric stamps but pruning sorts names lexically, which differs for older unpadded names the recogniser accepts. Enumeration silently drops unreadable entries.
Reported by: host-fs-output.

## BUG-064 - Duration and location rendering edge cases

Sites: `src/output/console.rs`, `src/diagnostic.rs`.
Near 60 seconds the common renderer can round tenths to `60.0s` before switching units. `Diagnostic::location` drops the line when the column is absent and bypasses `output::path`.
Reported by: host-fs-output.

## BUG-065 - env and bench dry run write the hash cache

Sites: `src/bench/params.rs`, `src/preflight/digest.rs`, `src/datasets/status.rs`, `src/env.rs`, `docs/commands/bench.md`.
Bench dry run (`PinsOnly`) calls the caching digest, so a cache miss creates `.brokkr/hash_cache`, its lock and a staging file, although help says it stores nothing. Dry run also skips the input hold. `env` calls dataset status, which can populate the cache, although it describes itself as touching nothing.
Reported by: bench, host-fs-output.

## Bench, stores and results

## BUG-066 - Results table shows block wall instead of child wall

Sites: `src/bench/results/mod.rs`, `src/bench/store/read.rs`, `src/bench/store/document.rs`.
The table loads runs with `children = false`. SQLite selects the best iteration by measured-child wall, then the renderer recomputes wall from that iteration, finds no children, and falls back to the whole block. The same run shows a different time in its publication line, UUID detail, comparison or rounds summary.
Reported by: bench.

## BUG-067 - A many-choices default with ".." loads and then fails to bind

Sites: `src/config_parts/bench.rs`, `src/bench/params.rs`.
A `many = true` param with `choices = ["a..b"]` and `default = "a..b"` passes the membership check at load. The binder reads `..` as range syntax and rejects it, contrary to the loader's promise that a default that loads will bind.
Reported by: bench.

## BUG-068 - Remote run reader trusts identity and indices

Sites: `src/bench/results/mod.rs`, `src/sidecar_cmd/bench.rs`, `src/bench/store/read.rs`.
The remote reader checks only the answer variant and `children_loaded`. It accepts a different run UUID or project, or a `best_iteration` outside the vector, which the default sidecar selection then indexes directly. The local DB reader bounds-checks it.
Reported by: bench, results-server-and-stores.

## BUG-069 - Short-ID helpers slice bytes and panic on multibyte ids

Sites: `src/db/types.rs` (`short_uuid`), `src/sidecar_fmt.rs`, `src/invalidate_cmd.rs`, visual report paths.
UUID and hash abbreviators slice by byte length, and historical SQL fields and `RunId::new` accept unrestricted text, so a multibyte character crossing byte eight panics in preview, report and sidecar views.
Reported by: bench, results-server-and-stores, worktree-clean-git-deps, visual-projects.

## BUG-070 - Host trace window mixes clocks

Sites: `src/bench/runner/host.rs`, `src/bench/lua.rs`, `src/bench/results/host.rs`.
The runner saves a `CLOCK_BOOTTIME` block start, and the renderer adds the Lua block's `Instant` elapsed wall to it. A suspend during the block puts the computed end too early and drops readings.
Reported by: bench.

## BUG-071 - Bench evidence status errors

Sites: `src/bench/runner/preflight.rs`, `src/bench/runner/host.rs`, `src/bench/runner/mod.rs`.
Preflight folds `output_cut` into `stderr_truncated` only, so losing the end of stdout looks like a stderr problem. Real capture checks raw bytes against the per-file cap but can retain a serialised report larger than that cap. A failed trim reap returns `DevError::Reported` whose message was never emitted.
Reported by: bench.

## BUG-072 - Typed sidecar --run returns another run's samples

Sites: `src/sidecar_cmd/mod.rs`, `src/db/sidecar.rs`, `src/sidecar_cmd/bench.rs`.
Typed selection parses `--run` as `usize` without checking `total_runs`. An index above `i64::MAX` binds as `0` for samples, markers and counters but as `i64::MAX` for truncation, calls and sink records. A representable missing run still gets a provenance header from the best run. Bench selection rejects out-of-range values.
Reported by: results-server-and-stores.

## BUG-073 - Typed provenance "wall" is the best run's sample span

Sites: `src/sidecar_fmt.rs`, `src/sidecar_cmd/bench.rs`.
The formatter always loads the best run's samples, subtracts first from last, truncates to whole seconds and labels the result `wall`, even for `--run N`. Script-bench provenance uses the chosen child's real wall. Typed timestamps go through the viewer's local timezone with no offset shown and `localtime_r` unchecked, while bench and history use UTC.
Reported by: results-server-and-stores.

## BUG-074 - Read-only typed-store opens migrate, and an old migration deletes data

Sites: `src/invalidate_cmd.rs`, `src/db/store.rs`, `src/db/schema.rs`, `src/db/migrate.rs`.
`invalidate` preview is documented as neither migrating nor writing, but it calls `RunStore::open_read_only`, whose typed opens migrate older files and can recreate a DB that vanished between opens. The v8-to-v9 results migration drops the embedded sidecar tables without transfer, and that runs on the read path.
Reported by: results-server-and-stores, worktree-clean-git-deps.

## BUG-075 - Typed sidecar decoders invent values

Sites: `src/db/sidecar.rs`, `src/db/query.rs`, `src/db/migrate.rs`.
An unknown `ending` decodes to absence, and the legacy exit code is consulted only when the column is `NULL`, so a missing or unknown terminal state can become `Sealed`. Missing sealing fields become empty name and zero, and malformed drop counts become `u64::MAX`. KV rendering prefers integer, then real, then text, while the SQL filter and the v25 redaction prefer text first.
Reported by: results-server-and-stores.

## BUG-076 - Phase summary and comparison disagree on CPU time

Sites: `src/sidecar_fmt.rs`, `src/sidecar.rs`.
Summary clamps user and system deltas independently. Compare subtracts combined sums and can go negative, which renders as no value. Both use unchecked arithmetic, and field stats sum `i64` before converting, so extreme stored counters can panic or wrap. Timeline arithmetic is unchecked too.
Reported by: results-server-and-stores.

## BUG-077 - Sidecar writes narration into its JSONL stream

Sites: `src/output/console.rs`, `src/sidecar_cmd/mod.rs`, `src/main_parts/bootstrap.rs`.
Stored-capture and clock warnings use ordinary `warn`, and empty-query notices use `result_msg`, but the sidecar command doesn't reserve stdout. Typed capture warnings omit the UUID and run index.
Reported by: results-server-and-stores.

## BUG-078 - Combined results queries disagree about candidates

Sites: `src/results_cmd.rs`, `src/sidecar_cmd/mod.rs`, `src/db/query.rs`, `src/db/format/table.rs`.
`--limit` applies separately to typed and script-bench lists with different recency and no global sort. The remote store is probed even for filters that exclude script benches. The early "no results yet" branch hides a sidecar-only UUID when `results.db` is absent. Typed rows load without children, so `captured_env` is empty although there is an env column and the SQL filter matches env. The grouped branch skips the documented library-versus-CLI comparison.
Reported by: results-server-and-stores.

## BUG-079 - Deploy identity check accepts uppercase and then compares case-sensitively

Sites: `src/bin/results_server/deploy.rs`.
The validator accepts an uppercase hex build, which then fails the case-sensitive comparison against the lowercase emitter. Deploy sends no protocol header, although the wire reference says every request does.
Reported by: results-server-and-stores.

## BUG-080 - HTTP response builder falls back to an empty 200

Sites: `src/bin/results_server/http.rs`.
The fallback silently replaces an invalid response with an empty default response, whose status is 200.
Reported by: results-server-and-stores.

## BUG-081 - Marker producers disagree on the tag channel

Sites: `../ratatoskr/crates/app/src/harness/mod.rs`, `src/sidecar.rs`.
Ratatoskr reserves `@` but allows `%` at the start of a marker name, which brokkr reads as the tag channel. Cross-repo, filed as ratatoskr#5.
Reported by: results-server-and-stores.

## Worktree, deps and crit

## BUG-082 - ccu prints an all-clear on an incomplete response

Sites: `src/deps/ccu.rs`, `src/deps/render.rs`.
A schema-1 response with no `checks` deserialises as an empty list and prints "All direct deps are at latest". Entries with no installed version or severity are omitted even when a latest version is known. An unknown severity discards both phases. Missing tool, malformed wire, timeout and crash all become `ToolMissing`. A pre-epoch clock suppresses the upgrade phase, which needs no clock.
Reported by: worktree-clean-git-deps.

## BUG-083 - deps --json shares stdout with operator output

Sites: `src/main_parts/bootstrap.rs`, `src/deps/render.rs`, `src/deps/focus.rs`, `src/deps/mod.rs`, `docs/commands/deps.md`.
Both renderers print NDJSON directly, but stdout isn't reserved, so lock waits, fresh-hold warnings and errors share the stream. `--no-fail` is documented as always exiting zero, yet metadata, config and render failures still error. Focus puts the display path spelling into JSON.
Reported by: worktree-clean-git-deps.

## BUG-084 - Duplicate-version detection miscounts

Sites: `src/deps/duplicate_version.rs`, `src/deps/mod.rs`, `docs/commands/deps.md`.
Package ids are grouped by name and the count is checked before versions or normal-edge reachability. Two sources at the same version show as multiple versions, and dev/build-only nodes count despite the documented Normal filter. The docs say the phase calls Cargo with `--filter-platform`, but the code projects in-process with the triple of `rustc` on `PATH`.
Reported by: worktree-clean-git-deps.

## BUG-085 - Focus and publication analysis edge cases

Sites: `src/deps/focus.rs`, `src/cargo/metadata.rs`, `src/deps/publish_cycle.rs`.
An empty focus name falls through to substring matching every package. The shared BFS visits each nonmember once, so converging routes lose chains that focus promises. Publication analysis matches members by name only, and reads `req == "*"` as no `version` key, losing an explicit `version = "*"`.
Reported by: worktree-clean-git-deps.

## BUG-086 - cargo clean -p reports the last stderr line as its summary

Sites: `src/clean.rs`.
The last stderr line is taken as the summary, so an unrelated trailing warning can be shown as the result.
Reported by: worktree-clean-git-deps.

## Visual projects and datasets

## BUG-087 - Sluggrs approval can pair one run's row with another render's image

Sites: `src/sluggrs/cmd.rs`.
The renderer writes a mutable `output.png`. Approval checks the file exists and that the latest row has acceptable provenance. If a later render writes the file and stops before its row, approval uses the earlier row with the later bytes. A manually replaced image also passes. A first-run `NO_BASELINE` row never decodes the image, so invalid PNG bytes can reach approval.
Reported by: visual-projects.

## BUG-088 - Sluggrs baseline publication has two commit points

Sites: `src/sluggrs/cmd.rs`, `src/visual/db.rs`.
Approval updates SQLite before renaming `approved.png`, so a kill or rename failure leaves a new record with old or missing bytes. A PNG with no record still acts as the comparator. `visual-status` consults only the DB. Sluggrs has no completion marker, so rows from a partial batch qualify as latest.
Reported by: visual-projects.

## BUG-089 - Litehtml publish can retire a good reference for invalid bytes

Sites: `src/litehtml/reference.rs`, `src/litehtml/cmd.rs`.
`inspect` checks existence, width and a nonempty id, so `publish` can retire a good reference for an invalid PNG or layout dump, and a hand edit that keeps the id passes freshness. The three renames assume one filesystem, so `EXDEV` can arrive after the old marker is gone. The unlocked dashboard can show transient `STALE`. Publication removes current metadata before renaming, so an intermediate failure leaves no complete reference.
Reported by: visual-projects, host-fs-output.

## BUG-090 - Printed visual run handle can't be passed to report

Sites: `src/cli/schema.rs`, `src/visual/db.rs`, `src/litehtml/cmd.rs`, `src/sluggrs/cmd.rs`.
Banners print an eight-character id and clap advertises a prefix, but both `report` paths query by exact equality. A run with no rows reports "no results found" before the completion diagnostic. The litehtml report omits commit, width, reference id and stored reasons.
Reported by: visual-projects.

## BUG-091 - Visual status hints and docs contradict the scorer

Sites: `src/visual/status.rs`, `src/visual/engine.rs`, `docs/commands/visual.md`.
The `NO_BASELINE` hint suggests bare `brokkr approve`, which the handler refuses without an id or `--all`. The docs list `NO_BASELINE` for an unapproved litehtml subject at a changed width, but the scorer returns pass, threshold failure or expected failure there. The docs call the ratchet the gate, but `determine_status` checks absolute thresholds first. Litehtml reports improvement from pixels alone. The summary emits `ExitCode(1)` instead of the shared failure constant.
Reported by: visual-projects.

## BUG-092 - Litehtml layout score reports perfect matches without evidence

Sites: `src/litehtml/compare.rs`, `../litehtml-rs/src/lib.rs`.
Missing coordinates are allowed, and when both relative coordinates vanish their delta is zero. Duplicate paths collapse to the last entry. A zero denominator returns 100%, pipeline-only boxes never enter it, and missing non-inline boxes lower the score without reaching `offenders.txt`. Equal-severity offenders take hash-map order. The sibling interpolates `id` and `classes` into JSON by hand, so a quote turns a valid fixture into a parse error (filed as litehtml-rs#2).
Reported by: visual-projects.

## BUG-093 - Sluggrs renderer metadata is parsed from log text

Sites: `src/sluggrs/cmd.rs`, `../sluggrs/examples/snapshot.rs`.
The reader takes the first line starting with `{`, parses only `adapter`, accepts an empty name and drops `backend`. The sibling interpolates JSON, so an adapter name with a quote turns a good render into `ERROR`. Only the first adapter is recorded. With no suitable GPU the sibling panics. The sibling side is filed as sluggrs#4.
Reported by: visual-projects.

## BUG-094 - Prepare and extract alter the documents they process

Sites: `scripts/litehtml-prepare/prepare.js`.
Documented external-CSS removal handles one `@import url(...)` shape, and `cmdPrepare` doesn't remove stylesheet links. The attribute serialiser escapes only quotes. Extraction puts all sibling cell stubs before the selected cell, which reorders columns, and drops body attributes and `<colgroup>` context. Outline emits raw ids and classes as selectors and assumes ids are unique.
Reported by: visual-projects.

## BUG-095 - Chrome capture and image fetch URL handling

Sites: `src/litehtml/capture.js`, `scripts/litehtml-prepare/prepare.js`.
Capture concatenates `file://` with a raw path, so URL-significant characters change what Chrome opens, and `browser.close()` runs only on success. Redirect handling recognises `/`-relative targets by concatenation, so path-relative and scheme-relative redirects resolve wrongly. The image cache writes before the caller proves the response is an image, so an invalid 200 persists as a hit, and the hit path accepts up to 128 MiB while the network refuses over 20 MiB.
Reported by: visual-projects.

## BUG-096 - Dataset validation and inventory misfire

Sites: `src/config_parts/parser.rs`, `src/datasets/status.rs`, `src/bench/params.rs`.
`sha256` rejection walks every nested key and fires on a dataset or variant named `sha256`, and its remedy names `xxhash` where plain paths use `xxh128`. `env` runs the file-or-tree digester on `file` entries, so it can report a directory as present or verified while bench binding needs a regular file. Inventory labels can collide (a PBF variant `osc.1` and OSC sequence `1`), and a registry holding only an empty snapshot produces no row.
Reported by: visual-projects.
