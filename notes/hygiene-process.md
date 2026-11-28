# Hygiene: lock, processes, reservations, check and test runners, convention gates, serve, redaction

Findings from the hygiene hunt: values with more than one owner, and assumptions about the host. Sibling files: `hygiene-host.md`, `hygiene-cargo.md`, `hygiene-stores.md`. Defects are in `bugs.md`. Nothing here has been verified.

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

## Redaction

## PROC-001 - The redaction perimeter has many gaps

Sites: `src/run_log.rs`, `src/history.rs`, `src/history_cmd.rs`, `src/lock/hold_protocol.rs`, `src/lock/mod.rs`, `src/lock/collector.rs`, `src/process/spawn.rs`, `src/process/inflight.rs`, `src/check_cmd/watchdog.rs`, `src/cargo_env.rs`, `src/check_cmd/activation.rs`, `src/check_cmd/binaries.rs`, `src/check_cmd/prepare.rs`, `src/check_cmd/conventions.rs`, `src/script_check.rs`, `src/check_cmd/fixture_lint/lock.rs`, `src/check_cmd/diagnostics.rs`, `src/cargo_json.rs`, `src/textlint.rs`, `src/passthrough_cmd.rs`, `src/install_step/mod.rs`, `src/launch/mod.rs`, `src/launch/probed.rs`, `src/version_cmd.rs`, `src/env.rs`, `src/crit_cmd/mod.rs`, `src/deps/ccu.rs`, `src/serve.rs`, `src/bin/results_server/deploy.rs`, `src/db/format/single.rs`, `src/db/migrate.rs`, `src/bench/runner/host.rs`, `src/bench/runner/stored_config.rs`, `src/bench/external.rs`, `src/bench/results/host.rs`, `scripts/litehtml-prepare/prepare.js`, `src/sluggrs/cmd.rs`, `src/test_runner/hung.rs`, `src/preflight/digest.rs`.
History and bench records redact classified credentials in argv. Elsewhere:
- The run-log header writes raw argv joined with spaces (boundaries lost) plus the absolute cwd, with no private file mode.
- The holder record (mode 0644 subject to umask), socket and queue carry full argv, roots and Codex ids, and `lock` and `kill` print them.
- The lock collector clips and strips controls from `/proc` cmdlines without redacting, while the watchdog's reader redacts. Its locked path and `comm` fallback skip argv sanitising, and clipping before redaction can expose a partial value.
- `[cargo_env]` logs `NAME=value` on success, and the failing command and up to 16 KiB of stderr on failure. Inline conflicts print the full `--config` assignment, and env conflicts print both values.
- Runtime fingerprint drift prints raw build-script `rustc-env` pairs. Rustdoc command lines include the composed rustdocflags.
- Script pass notes strip some controls and redact, failure replay redacts without that normalisation, and `expect` is printed raw on mismatch.
- Fixture seed, clean and clippy failures, textlint excerpts and malformed-Cargo-JSON reports quote raw input.
- Passthrough logs forwarded argv. Install prints the whole `sh -c` string. Deploy prints restart argv. Version prints remote stamp text with control characters.
- `env` prints raw mount sources and options.
- Crit echoes harness arguments and stores raw flags. Deps prints Git and registry URLs, absolute manifest paths and raw `ccu` stderr. Serve replays raw log tails, and its log uses ambient permissions.
- Probe, peer and serve environment files are 0600, but a killed writer leaves copies, and the plan file goes through a plain `fs::write`. The nextest env file is the same.
- The typed v25 scrub covers structured `brokkr_args` and `env.*`, but not older strings or `cli_args`, and it calls the live redactor, so a pre-v25 file migrates differently after a policy edit.
- Bench stores composed config, source, param keys, flag placement, Lua prints, stderr tails, records, reports and mark reasons. External lookup reports the full PATH on a miss. The previous history command is copied into the next bench document raw.
- Timeout snapshots persist raw syscall fields and thread names.
- Prepare warnings print full (possibly signed) URLs, sometimes twice, plus data-URI prefixes. Sluggrs replays raw child streams.
- Hash-cache keys hold canonical absolute paths.
The URL credential echo is a defect in `bugs.md`.
Enforcement: sensitivity-typed argv, env, URL and path fields with one redaction and escaping boundary before console, log, persistence and upload, private modes for diagnostic files, and sentinel-secret tests through every sink. Arbitrary child output can't be reliably scrubbed by pattern and needs an explicit capture and retention policy.
Reported by: all thirteen scopes.

## Child environment and launch

## PROC-002 - Reserved child-environment keys have no single catalogue

Sites: `src/process/spawn.rs`, `src/config_parts/parser.rs`, `src/check_cmd/nextest_lane.rs`, `src/launch/probed.rs`, `src/main_parts/bootstrap.rs`, `src/serve.rs`, `src/serve/supervisor.rs`, `src/bench/lua.rs`, `src/bench/runner/host.rs`, `src/test_runner/run.rs`, `src/test_runner/harness_shim.rs`, `src/test_runner/output.rs`, `src/lock_cmd.rs`, `reference/check-internals.md`.
The sweep validator rejects the nonce, lease marker and cache switch. Nextest filters the nonce, cache switch, orphan token and `BROKKR_EXEC`, but not the lease marker. `command` applies hold policy before spawn, normal spawn restamps cohort and capability, and full-env spawn keeps the supplied env and transfers the reservation fd without an orphan token, which the frozen bench caller adds by hand. Frozen detach removes the harness dir key while the detached spawn list omits it. `BROKKR_HARNESS_ARTEFACT_DIR` is a constant in launch and literals in bootstrap and serve. `BROKKR_HARNESS_SOCKET` is a literal at install and a constant in the shim, and the socket basename and runner marker are spelled twice. Shim strictness is presence-based, so an inherited empty or `0` is strict. Scrubbing drops `RUSTDOC` unconditionally and finds runner overrides by substring. `BROKKR_CARGO` accepts any nonempty value while operator text says `=1`. `BROKKR_SERVE` presence, even empty, triggers nested-start refusals while carrier cleanup needs the exact token. Forced Cargo `[env]` and nextest build-script env can displace transport values. Lua refuses caller `BROKKR_*` and marked-call `HOTPATH_*`, which makes some branches redundant. See also `hygiene-stores.md` (bench call precedence).
Enforcement: define each key once with ownership, empty-value meaning, inheritance and removal rules. Render it per engine and test every launch mode with missing, empty, inherited and explicit values. A text rule for raw key spellings.
Reported by: test-runner, launch-and-install, lock, process-and-reservations, results-server-and-stores, config-cli-startup, worktree-clean-git-deps, bench.

## PROC-003 - Executable lookup has several owners

Sites: `src/bench/runner/preflight.rs`, `src/bench/runner/host.rs`, `src/bench/external.rs`, `src/process/spawn.rs`, `src/launch/helper_main.rs`, `src/check_cmd/direct_runtime.rs`.
Bench preflight and measured calls each pick the first explicit `PATH`, then `std::env::var("PATH").ok()`: empty searches the child cwd, and unset or non-UTF-8 fails lookup, while the spawned child can still inherit a byte-valued PATH. The launch helper falls back to `/bin:/usr/bin`. `freeze_env` and `Exact::of` each rebuild environment-plus-overrides. Exact spawn's "absolute path" contract is a comment on a `&str`. The resolver uses `access(X_OK)` and then opens to digest, and execution opens again, so the recorded digest can describe a different file. Shebang interpreters aren't part of the digest. Hashing happens before the child clock starts.
Enforcement: resolve once from an explicit OS-string env and cwd, use an absolute executable type, and a descriptor-backed exec where provenance is promised.
Reported by: process-and-reservations, bench.

## PROC-004 - Launch lease handoff goes through a thread-local slot

Sites: `src/reservations/launch.rs`, `src/process/spawn.rs`, `src/bench/runner/host.rs`.
Admission arms a thread-local slot with a raw fd and a program spelling. Another admission overwrites it, an intervening launch of the same program consumes it, and another thread misses it or leaves a stale fd. Spawn succeeds with no matching slot. Bench evidence decorates `children.last_mut()` after push.
Enforcement: an owned single-use lease passed into the command, a child handle that owns release, and a per-call evidence builder.
Reported by: process-and-reservations, bench.

## PROC-005 - Publication and child tracking depend on call order

Sites: `src/build.rs`, `src/output/child.rs`, `src/maintenance.rs`, `src/lock/mod.rs`, `src/lock/activity.rs`.
Build looks up the ambient hold, publishes a child PID, runs a callback and clears after it returns. Passthrough publishes and clears separately. Probe preparation, snapshots and install recovery can mutate without a lock argument. `set_child_pid` keys slots by thread, so a second child on one thread replaces the first, and poisoned state skips publication silently. `Activity` is movable across threads although its drop edits the dropping thread's TLS.
Enforcement: lease-bound writer APIs, a registration token per supervised child, and a thread-bound TLS guard.
Reported by: launch-and-install, lock.

## Processes, signals and reaping

## PROC-006 - Process identity verification sits outside the signalling API

Sites: `src/process/reap.rs`, `src/process/mod.rs`, `src/stray.rs`, `src/test_orphans.rs`, `src/shutdown.rs`, `src/lock/collector.rs`, `src/check_cmd/watchdog.rs`, `src/lock_cmd.rs`.
`VerifiedProcess::open` reads `/proc/PID/stat` before and after `pidfd_open`. With procfs mounted from an ancestor namespace, the reads verify one process while the syscall reads the number in the caller's namespace. Stray, orphan and watchdog reaping reach `kill_verified` without the holder verification `kill --hard` uses. The collector sometimes infers the namespace from equality with `getpid`, and the watchdog walks from `getpid`. Missing namespace tokens enable a legacy procfs-only check. A pidfd pins a process generation, not ownership of an acquisition: a holder can release between scope lookup and numeric SIGTERM or SIGSTOP, and hard kill resumes its descendant walk from a bare root PID.
Enforcement: require a verified local PID view before building a signalable identity, and make termination an acquisition-scoped operation. Namespace fixtures.
Reported by: process-and-reservations, lock.

## PROC-007 - The stray reaper's selection is weaker than its delivery

Sites: `src/stray.rs`, `docs/commands/check.hygiene.md`.
Between `find` and `kill` a stray can exit and its PID be reused. Descendant expansion starts from the old PID in a fresh table, and rust-analyzer starters are signalled using the fresh row's identity without checking generation or name. Selection uses broad, kernel-truncated `comm` prefixes, so a locked command can kill same-user Cargo jobs in other repos and the editor's rust-analyzer. The formatter exemption mistakes option values for the subcommand, and expansion ignores it under a selected root. `current_ownership` collapses unreadable lock status into no holder, and `EPERM` from `NS_GET_PARENT` is read as outside the namespace. The summary counts only root signals, and `strays --kill` succeeds even when nothing was signalled. Reaping runs from both the drain and the post-hold path.
Enforcement: destructive cleanup only on positive containment or an operator-selected scope, a typed `Unknown`, and a typed reap report.
Reported by: process-and-reservations, lock.

## PROC-008 - Environment readers for /proc/PID/environ disagree

Sites: `src/process/helpers.rs`, `src/test_orphans.rs`, `src/serve/carriers.rs`, `src/lock/collector.rs`, `src/lock_cmd.rs`.
The generic parser skips a first match with invalid UTF-8 and can return a later duplicate. The orphan parser takes the first match, then decodes it. Serve compares the first match as bytes and silently skips unreadable processes. Lock observation caps at 1 MiB and can accept a cut final fragment. Kill's reader reads the whole block and collapses absent with failure.
Enforcement: one byte-oriented reader that keeps first match, truncation and I/O status, with each marker validating its own value.
Reported by: process-and-reservations, lock, worktree-clean-git-deps.

## PROC-009 - Reapers repeat traversal and retry policy

Sites: `src/stray.rs`, `src/shutdown.rs`, `src/process/inflight.rs`, `src/test_orphans.rs`, `src/serve/carriers.rs`, `src/check_cmd/watchdog.rs`, `src/lock/hold_protocol.rs`.
Stray selection walks 256 ancestors, expansion stops at 64, shutdown at 65,536 rows, and the in-flight report has its own copy of that bound with no visited set. The watchdog and hard kill do two sweeps, and serve and orphans do three passes with a one-second settle. Orphan settling polls bare PIDs every 5 ms and orders by start ticks, which can tie for parent and child. Signal, identity, flock, unlink and enumeration errors are mostly discarded, and the summary lists unbounded names without PIDs or reasons. The sweep runs on every fresh hold and delays all lock users. The orphan census sees only the local PID namespace, while its flock answers across namespaces.
Enforcement: one indexed snapshot, explicit traversal and retry policies, stable identities through settle, and a bounded typed report.
Reported by: process-and-reservations, test-runner.

## PROC-010 - Group signalling and pidfd wrappers have diverged

Sites: `src/lock_cmd.rs`, `src/test_runner/leader.rs`, `src/process/pidfd.rs`, `src/process/reap.rs`, `src/output/child.rs`, `src/shutdown.rs`.
Lock kill rechecks a leader, then uses numeric `kill(-pgid)` with a documented reuse race. The test supervisor uses pidfd group signals where supported. `GroupReaper` registers a bare PGID, silently gives up after 256, and a handler can use a number after it's reusable. The own-child pidfd casts `u32` to signed, the verified one range-checks and collapses failures, and captured supervision falls back silently and warns without PID or program. `kill_descendants` returns a count and drops failures, and `PidfdRefusal::OpenFailed` drops errno. The defects are in `bugs.md`.
Enforcement: shared capability detection, typed signal outcomes, a valid process-group type, and a supervisor that owns both handle and reap.
Reported by: lock, process-and-reservations, test-runner.

## PROC-011 - Signal dispositions have two writers

Sites: `src/shutdown.rs`.
`SigtermGuard` and `GroupReaper` use different mutexes, both install SIGINT and SIGTERM handlers, and infer each other's ownership from a third state. The last guard restores defaults before reinstalling the reaper, leaving a window with live groups and default handlers. The guard discards prior dispositions while the reaper preserves ignored signals. Escalation changes the disposition inside a handler, so a second stop request can become default termination during cleanup. `is_shutdown_requested` is an alias for `should_unwind`, and both have callers.
Enforcement: one signal controller from startup, with policy as observed state, and subprocess transition tests.
Reported by: process-and-reservations, lock.

## PROC-012 - Stop requests and the launch gate are separate operations

Sites: `src/shutdown.rs`, `src/process/spawn.rs`.
The socket commits a stop and closes the mmap-backed gate, but a watchdog or guarded signal can set the request flag while `admits_new_work` still permits launches. The gate caches mmap failure for the process, assumes a 4,096-byte mapping and loses errno. Ordinary and exact spawn each implement the refusal marker.
Enforcement: stop scope and spawn stage in one launch protocol, with preserved syscall errors.
Reported by: process-and-reservations.

## PROC-013 - Parent-death and OOM protection degrade silently

Sites: `src/shutdown.rs`, `src/oom.rs`, `src/bin/fstrim_helper.rs`.
Ordinary parent-death setup proceeds when `prctl` fails, while the trim helper refuses it. Setup is tied to the spawning thread and covers only the direct child. `protect_child` writes the opaque `1000` and ignores open, short-write and close results.
Enforcement: each launch purpose declares required or best-effort setup, and the child returns a typed outcome.
Reported by: process-and-reservations.

## PROC-014 - Supervisor deadlines are deadlines to start termination

Sites: `src/output/child.rs`, `src/test_runner/run.rs`, `src/test_runner/procgroup.rs`, `src/fs_probe.rs`, `src/test_runner/libtest_json.rs`.
The supervisor issues signals, discards delivery errors and can block in `Child::wait`. Drain settling joins normally after its grace. `fs_probe` and test `procgroup` each run a 100 ms `poll`/`read` loop. `read_unless_cancelled` maps EOF, cancellation and OS errors to `None`, and the JSON path keeps `ReadError` without the cause. Both supervisors give pipes two seconds, then join readers, and a test drain calling a blocked output callback isn't bounded by cancellation. `run_listing` returns directly on `try_wait` failure, dropping handles without cancelling drains. `run_libtest` registers its signal group after starting shim setup. A normal leader exit reaps at once, while serve kills the group first. A pidfd that's missing degrades to polling.
Enforcement: one cancellable drain primitive with EOF, error and stopped results, one lifecycle owner that reports incomplete reap, and presentation kept off reader threads.
Reported by: test-runner, process-and-reservations, host-fs-output, convention-gates.

## PROC-015 - Capture is bounded per child at best

Sites: `src/output/child.rs`, `src/test_runner/procgroup.rs`, `src/test_runner/run.rs`, `src/check_cmd/parallel.rs`, `src/check_cmd/conventions.rs`, `src/check_cmd/concurrency.rs`, `src/script_check.rs`, `src/test_runner/harness_shim.rs`, `src/test_cmd/run.rs`, `src/test_runner/evidence.rs`.
Ordinary Cargo streams have byte caps. The test runner appends to growing vectors, and so do newline assembly, shim blocks, issue lists, tracked names, repeat buffers and final clones, with listings parsed from them. Parallel orchestration keeps each binary's capture until the lane reports. Serial script failures keep full captures until the stage reports, background ones keep every outcome until join, and note collection stores every match before keeping five. The shim keeps every admitted harness's pidfd, tracker and identity. `test -N` accepts any `u32` and builds every iteration's inventory up front. The evidence collectors' dispatch queue is unbounded.
Enforcement: byte, line, record and identity budgets at each owner, bounded queues, and inventory shared across repeats. Flood and many-entry tests.
Reported by: check-orchestration, test-runner, convention-gates, host-fs-output.

## PROC-016 - Lock-wait diagnostics are another kernel parser

Sites: `src/process/inflight.rs`, `src/fstrim.rs`.
It excludes negative owner PIDs (the open-file-description sentinel), reports only the first blocked row per PID, then reads cmdline and fds by bare PID. It splits device numbers independently of fstrim's `libc` helpers.
Enforcement: read evidence through a process handle, keep owner-unavailable apart from no-lock, and use one device-number type.
Reported by: process-and-reservations.

## PROC-017 - Dead or redundant process and spawn APIs

Sites: `src/reservations/launch.rs`, `src/bench/runner/preflight.rs`, `src/db/historical_qualifier.rs`, `src/output/child.rs`, `src/clean.rs`, `src/clean/layout.rs`.
`LaunchSpec`'s only production constructor (preflight) passes empty read and write lists and no private dir, so the general placement check, private-dir prefix, argv substitution and disposable-file path are test-only. `command_status` is test-only, though the docs present it as general. Retired nidhogg ingest and pbfhogg verify paths remain cleanup operands with no writer. V1 lease records have recovery tests.
Enforcement: replace preflight with the empty child lease it needs, and keep historical decoding with the readers.
Reported by: process-and-reservations.

## Lock and guard

## PROC-018 - Lock identity and the state directory are taken on trust

Sites: `src/user_dirs/paths.rs`, `src/lock/mod.rs`, `src/bin/rustc_guard.rs`, `src/lock/service.rs`, `src/lock/queue.rs`, `src/user_dirs.rs`.
Lock and compile-lock opens follow symlinks and don't check file type or owner, so an external delete splits the lock and a symlink redirects record writes. Queue and socket operations resolve directory paths, and the socket directory is chmodded. `user_dirs::queue_dir_at` and `queue_dir_for` each build the queue root, agree today, and the latter gives any two locks in one directory a shared index. A filesystem Unix socket under `$HOME/.brokkr/lock` makes a long home, a socket-less filesystem or bind permissions fatal, and chmod failure is ignored. A shared home across machines can give the same flock to processes whose PIDs and sockets belong to different kernels. See also `hygiene-host.md` (HOME).
Enforcement: a private state-directory descriptor with validated descriptor-relative opens, one layout value for lock, queue, compile lock and socket, and a short host-local runtime dir for endpoints.
Reported by: lock.

## PROC-019 - Lock inspection has write authority

Sites: `src/lock/mod.rs`, `src/lock/service.rs`, `src/main_parts/bootstrap.rs`, `src/lock/hold_protocol.rs`.
`status()` creates the state directory and opens the lock with `O_CREAT | O_RDWR` after an `exists()` check, so it can recreate a vanished lock and needs write access to inspect. The client creates and chmods the socket directory through the server's helper. `lock`, `kill` and `guard` record history. Even recovery's lookup calls `lock_path()`, which can create directories and block before its bounded wait.
Enforcement: read-only opens and lookup-only socket paths for observation, and a read-only filesystem test.
Reported by: lock.

## PROC-020 - Queue order and liveness rest on mtime and a mutex over I/O

Sites: `src/lock/queue.rs`, `src/lock/hold_protocol.rs`, `docs/commands/check.hygiene.md`.
A record becomes visible to `kill` before its waiter has a ticket, and a later or nonblocking waiter can take `admit.lock` first. The reference defines arrival as ticket creation, the docs say join order. A cancelled ticket can leave an index entry. `admit.lock` covers directory scans, metadata, opens, flock attempts and ticket changes, so a stalled operation blocks everyone. Heartbeat liveness uses mtime: a forward clock jump forfeits a healthy waiter, a future mtime keeps a stopped one fresh, and heartbeat failures are discarded. Scans are unbounded and records are read to EOF. Sweeps swallow errors, and an unreadable index silently changes order. Queue defects are in `bugs.md`.
Enforcement: one queue transaction or coordinator with a sequence number and a monotonic clock, and interleaving tests.
Reported by: lock.

## PROC-021 - The holder file and socket publish different snapshots

Sites: `src/lock/mod.rs`, `src/lock/record.rs`, `src/lock/hold_protocol.rs`, `src/disk_gate.rs`.
A failed live metadata write clears file fields while `LockInner::publish` still sends the original snapshot, so status can show children hard kill has dropped. The file is rewritten in place (write then truncate) with double reads, which can match a paused partial write. `LockState` is held over process checks, writes, socket publication and warnings. Service setup and the disk gate run before the holder address is published, and release drops the service before restoring cgroup and tree, leaving windows with an unaddressable flock. The acquired line prints before drain and tree activation finish, though the docs say it avoids announcing a later-abandoned acquisition. The 120 s drain budget can be exceeded by synchronous reap, I/O or `EINTR`.
Enforcement: stable flock inode for exclusion, a bounded atomic acquisition-tagged snapshot published elsewhere, and one deadline through bounded operations.
Reported by: lock.

## PROC-022 - Holder, queue, socket and serve schemas each restate identity

Sites: `src/lock_record_codec.rs`, `src/lock/queue.rs`, `src/lock/service.rs`, `src/lock/hold_protocol.rs`, `src/lock_cmd.rs`.
Each represents project scope, root and identity. Queue decoding requires `configured=0|1`, while malformed values elsewhere become configured. Malformed progress is dropped in the file but rejects socket status. Malformed child entries can vanish before hard-kill preflight. Namespace capture is repeated, the file gets namespaces from all-or-nothing `HostScope` while socket and queue publish partial fields, and unconfigured kill scope compares lossy display strings.
Enforcement: typed `Scope`, `AcquisitionId`, `ProcessIdentity` and native paths in one versioned schema, with codec tests.
Reported by: lock.

## PROC-023 - The socket protocol is stringly typed with incomplete bounds

Sites: `src/lock/service.rs`, `src/lock/report.rs`, `src/shutdown.rs`.
Callers and server repeat `status` and `stop`, and reply tokens and `x1.*` keys are spelled on both ends. `read_line` accepts EOF termination, can exceed 256 bytes by a chunk, and can reject a valid first line over later invalid UTF-8. The 24 KiB extension limit leaves argv, root and ids unbounded, so a valid reply can exceed the client's 64 KiB. One service thread handles everything, the client's deadline starts after a blocking `connect`, and status does synchronous `/proc/self` reads first, so a slow status delays stop. `stop_with` holds `live` over the reply write and self-signal, and every stop sends SIGTERM again. Write, UTF-8, size and timeout failures collapse to generic text.
Enforcement: typed request and reply variants, a whole-frame limit before allocation, idempotent per-acquisition stop off the reporting path, and fragmentation tests.
Reported by: lock.

## PROC-024 - The workload cgroup works on a subset of hosts

Sites: `src/lock/cgroup.rs`.
Setup skips root, non-v2, non-domain, non-delegated, non-rooted or escaped mounts, mostly silently. `subdirs` resolves a descriptor back to a pathname. Cleanup assumes a matching empty `brokkr-<id>` belongs to brokkr. Restore drops errno and sets a process-global stranded flag. The member limit is checked before reading all of `cgroup.procs`.
Enforcement: a dedicated delegated subtree, descriptor-relative enumeration, typed coverage and strict limits.
Reported by: lock.

## PROC-025 - Guard enrolment and effective routing are different facts

Sites: `src/guard.rs`, `src/target_platform.rs`, `src/rustflags.rs`, `src/bin/rustc_guard.rs`, `src/build.rs`, `docs/commands/check.hygiene.md`.
Status reads one Cargo-home document, but Cargo's wrapper chain also consults project config, includes, `RUSTC_WRAPPER`, `CARGO_BUILD_RUSTC_WRAPPER` and workspace wrappers, and an empty higher-priority wrapper disables it. The report still says every invocation routes through the guard. The handshake probes the configured spelling from `/` while status checks existence from cwd. The guard derives its lock from its inherited `HOME`, so a different home admits compiles independently of the live hold. With the nested wrapper chain, the outer guard can see the real compiler as a positional argument and send a genuine probe down the compile path, where refusal can poison Cargo's probe cache. The cache hint recognises `--print` but misses a nested version probe. The guard advice hardcodes `target/.rustc_info.json`. The capability-list text is emitted but only the version is read. Probe failures on locked runs suppress config errors and run once per process. Enrolment defects are in `bugs.md`.
Enforcement: report enrolment and effective selection from Cargo's resolved context, a stable per-user lock identity, and real-toolchain fixtures for the wrapper composition.
Reported by: lock, cargo-integration.

## PROC-026 - Compile participation degrades silently

Sites: `src/bin/rustc_guard.rs`, `src/lock/hold_protocol.rs`, `reference/check-internals.md`.
Lease-open, flock and fd-flag failures collapse into one warning with no path or errno. A failure probing `brokkr.lock` after taking the lease becomes `Idle`. A missing lease directory permits unleased compiles. An unreadable auth record reads as a starting hold, and an empty nonce as another hold. The recursion marker is ambient text while the lease is a descriptor, so closing one without the other gives a false refusal or a drain that waits on its own ancestor. A retained descriptor can outlive the compiler in a detached helper.
Enforcement: keep fail-open as a typed result with evidence for each case, and track leases by acquisition through a supervision interface.
Reported by: lock.

## PROC-027 - The Python guard probe has drifted from the protocol

Sites: `scripts/guard-decision-probe.py`, `src/lock_probe.rs`, `src/bin/rustc_guard.rs`, `src/target_namespace.rs`.
It hardcodes the HOME layout, record keys, env names, cache setting, binary name and sample argv. Its "active" `auth=deadbeef` is shorter than the 32-character `AuthHash`, so the stale-auth and wrong-capability cases never exercise a valid hash, and any nonzero exit counts as refusal. Query cases run `/usr/bin/true`. It needs Python, `fcntl` and a host build, writes a persistent checkout-keyed dir under Cargo's target root, has no subprocess timeout, captures unbounded output, and can leave its scratch HOME after a hard stop.
Enforcement: generate fixtures with the Rust encoder, assert exact exit and lease outcomes, and put artifacts under a brokkr-owned cache.
Reported by: convention-gates, lock.

## PROC-028 - Retired lock fields still travel

Sites: `src/lock/mod.rs`, `src/lock_record_codec.rs`, `src/lock/service.rs`, `src/lock_protocol.rs`, `src/guard.rs`.
`mocks` has no production writer since its setters were removed, yet travels through state, file, socket and hard kill. `LockGuard::set_progress` is test-only. `threads` and `codex_session` have no consumer. Legacy single-child keys, whole-second CPU, namespace and configured fallbacks and the old stop-error grammar support older peers. The shared version is still `1`, so it doesn't prove capability behaviour.
Enforcement: remove unused fields after checking external consumers, with a versioned schema and a consumer test per field.
Reported by: lock.

## PROC-029 - Lock observation has several definitions of unknown

Sites: `src/lock/collector.rs`, `src/lock/report.rs`, `src/lock/service.rs`, `src/lock_cmd.rs`.
Missing tick rate becomes 100 in the collector and unavailable in the holder CPU figure. Unknown age and cgroup CPU become zero in some rows. The service reads `self_cpu` twice. The collector holds two full process scans and caches all rows, clipping only for the socket, while the local fallback renders all rows. Activity depth clips silently, and zombie counts depend on rows that survived the wire budget. Queue failures and SIGTERM delivery failures can show as `[lock]` progress. Stop lines omit acquisition and project, unreadable waiter rows omit their token, and a kill refusal joins targets into one long line.
Enforcement: typed known, unknown and truncated observations in one bounded report model, with one renderer.
Reported by: lock.

## PROC-030 - Lock test seams take ambient inputs

Sites: `src/lock/hold_protocol.rs`, `src/lock_tokens.rs`, `src/lock/service.rs`.
`HoldEffects`, path-explicit acquisition, `DrainPolicy`, `decide_with` and injected stop callbacks are useful seams. The lifecycle still takes clocks, sleeps, UID, cwd, argv, env, procfs and entropy internally. A socket test uses the real user socket directory and can create and chmod it.
Enforcement: a startup context with injected clock, entropy, procfs and sockets, and a disallowed-methods check verified with fixtures.
Reported by: lock.

## Reservations

## PROC-031 - The reservation transaction is a convention

Sites: `src/reservations/mod.rs`, `src/reservations/store.rs`, `src/reservations/guard.rs`, `src/worktree.rs`, `src/reservations/iteration.rs`, `src/bench/host_trace.rs`, `brokkr.toml`.
`Store::scan`, `create_tied` and `Lease::release_with` don't require a `Serial` token. The process mutex is taken before the 120 s flock loop with no timeout, and both locks can be held across scans, fsyncs, recursive removal, `git worktree remove`, release polling and an arbitrary observer. The host sampler holds its stop mutex while reading procfs and sysfs. The `reserved-mutations-through-the-gate` rule scans `atomic_write.rs` and bench code for three `fs::` spellings and accepts a marker in place of proof, so it can't see truncating opens, descriptor writes, other spellings, child mutations, or removals in clean or worktree.
Enforcement: make the transaction a required capability, and claim operands briefly before doing blocking work as the claim holder. Put destructive operations behind types that carry a reservation or a private-directory capability.
Reported by: process-and-reservations, bench.

## PROC-032 - The per-user store is resolved repeatedly

Sites: `src/reservations/store.rs`, `src/reservations/mod.rs`, `src/reservations/guard.rs`.
`Store::user` reads home for the durability anchor and again through `brokkr_state_dir` for the root, and `serialize` resolves the store separately from its callers. Lease creation refuses without a home, while mutation and clean reclamation proceed without a store on the assumption nobody else opened it.
Enforcement: pin one state root and reservation realm per invocation.
Reported by: process-and-reservations.

## PROC-033 - Path resolution can approve a different path than the one used

Sites: `src/reservations/guard.rs`, `src/reservations/paths.rs`, `src/reservations/launch.rs`, `src/bench/runner/mod.rs`, `src/reservations/iteration.rs`, `reference/bench-resources.md`.
Guard and read-hold fall back to `/` when `current_dir` fails. `Spelled::resolve` stops at the first missing component even if a later `..` backs out. There's no identity check for hard links, bind mounts or case folding, and a `statx` mount-id failure falls back to device equality. The run-level hold records spellings at admission, and later iterations resolve names again, so a veto can stop a write and still allow a read of different bytes under the old digest.
Enforcement: one resolver with explicit bases, pinned parent descriptors and identity for existing operands, with digest and read bound to one pinned input.
Reported by: process-and-reservations.

## PROC-034 - Private cleanup can delete a path it never owned

Sites: `src/reservations/launch.rs`, `src/reservations/store.rs`.
A private path is published before `mkdir`, and `identity: None` permits reclamation by path alone. If `mkdir` hits an existing name, `forget` tries to clear the record, and a failed rewrite followed by failed reclamation leaves a record that authorises deleting that path next scan. `remove_private` checks and removes through separate pathname operations.
Enforcement: a pinned parent owned by the allocator, with proposed, created and reclaimable as separate states.
Reported by: process-and-reservations.

## PROC-035 - The lease record reader accepts shapes the writer excludes

Sites: `src/reservations/store.rs`.
It validates versions and some tie combinations, but doesn't check that resource paths are absolute, private paths are under an owned root, borrows name a real owner, or identity matches disposition. It reads an unrestricted `String`, follows record symlinks and can block on a FIFO. `set_identity` and `forget` accept bad indices. A corrupt record can block all work or authorise removal of an arbitrary stored path.
Enforcement: a bounded versioned decoder over regular files under a pinned root, and typed writer handles.
Reported by: process-and-reservations.

## PROC-036 - Lease and orphan stores implement one disk protocol twice

Sites: `src/reservations/store.rs`, `src/test_orphans.rs`, `src/lock_tokens.rs`.
Both mint tokens from `/dev/urandom`, publish `.pending` then `.lock` under flock, and use a 60 s mtime grace. Reservations check lock type and final symlinks, separate contention from flock errors and sync entries. Orphans follow links, treat all flock failures as unavailable and have their own unlink handling. A future mtime keeps pending residue indefinitely.
Enforcement: shared publication, open and age primitives with injected filesystem, clock and entropy.
Reported by: process-and-reservations.

## PROC-037 - Iteration I/O has partial commits and copied policy

Sites: `src/reservations/iteration.rs`, `src/bench/runner/host.rs`.
Preflight `open_input` and `open_admitted` repeat flags, symlink translation and regular-file checks. Three inode checks turn inspection errors into "replaced". `write_file` unlinks the old entry before the new write completes. Scratch claims its name before admission succeeds, and call setup bumps its index and pins spools before the borrow is published. Preview returns a short result when a spool shrank, while tail refuses. Pending and temp cleanup drop errors, and bench spawn failure paths drop `finish()` results.
Enforcement: one owned-directory I/O API that commits after admission, and joint preview and tail semantics.
Reported by: process-and-reservations, bench.

## PROC-038 - Reservation resources grow without bound

Sites: `src/reservations/iteration.rs`, `src/reservations/store.rs`, `reference/bench-resources.md`.
A child can fill stdout and stderr spools on scratch. Every call pins two more descriptors and a child record. The two-second release window can't cap a child or its writes. Scans hold a descriptor per free lease and read unbounded records. Retained records and future-dated pending files accumulate.
Enforcement: per-call and per-iteration spool quotas, fd and call budgets, and bounded record input.
Reported by: process-and-reservations, bench.

## PROC-039 - Writes can escape the reservation boundary

Sites: `src/bench/runner/mod.rs`, `reference/bench-resources.md`, `src/clean.rs`, `src/worktree.rs`, `src/reservations/guard.rs`, `src/bench/runner/host.rs`.
An iteration container can be configured under a Cargo output dir or another tree clean removes without the gate, and placement screens one lexical case. Recursive private cleanup has no nested-mount policy. Backup protection is pbfhogg policy inside the generic gate: a basename prefix in one recursive walk, which another removal entry point skips, under the serial transaction with no work budget. Marker FIFOs go to project state outside the iteration lease, and need a writable source checkout. Children run with the live project root as cwd, so relative writes land in source or ignored folders. Lease cleanup warns for an ordinary live lease, reports no count on success, and its release-timeout text says private dirs are kept even for a borrow.
Enforcement: reservation-owned scratch on the configured filesystem, clean removals behind a capability, a pbfhogg-owned backup location, and child working files under lease scratch.
Reported by: process-and-reservations, bench.

## fstrim

## PROC-040 - The fstrim protocol is spelled in both binaries

Sites: `src/fstrim.rs`, `src/bin/fstrim_helper.rs`, `src/bench/store/document.rs`, `src/bench/runner/mod.rs`, `src/bench/runner/host.rs`, `src/bench/host_trace.rs`, `src/lock/mod.rs`, `docs/commands/fstrim.md`.
Both spell protocol `1`, the ext magic, arguments, stage words, capability messages and exit meanings. The handshake catches an installed mismatch but not same-version drift. The probe accepts trimmed version text and any `cap_sys_admin ` line on exit 1. The parent infers an unconfirmed reap from the English `wait failed` prefix and tests success via `"exit 0"`, which the bench document checks again, and the runner compares `"interrupted"`. The stored device predicate accepts fields larger than `u32` where the live parser doesn't. The fd floor `10` is in both fstrim and `LockGuard::duplicate_flock`, and `F_GETFD` / `F_SETFD` are repeated in reservation, detached spawn, fstrim and guard. The helper path is fixed at `/usr/local/libexec` and the hint assumes `~/.cargo/bin`. The probe checks only the capability, while seccomp, namespaces or a device refusing `FITRIM` fail later, and the magic can't certify the ext4 driver. Bench checks the scratch filesystem and retained iterations only at first reset, after the build, and prints an unbounded list. Probe and trim use `ChildLimit::None`. The host trace reads a fixed systemd timer stamp's mtime. The CLI target requires UTF-8, `open_target_at` accepts `.` and `..`, captured helper stderr is discarded, and a start error exits the loop while a syscall failure continues.
Enforcement: a dependency-free shared wire definition with parser fixtures, a deployment descriptor for path and hints, typed reset endings, and stable prerequisites checked before the build.
Reported by: launch-and-install, lock, process-and-reservations, bench, config-cli-startup, host-fs-output, worktree-clean-git-deps.

## Check runner

## PROC-041 - The watchdog journal has two writers

Sites: `src/check_cmd/accounting/journal.rs`, `src/check_cmd/accounting/reconcile.rs`, `src/check_cmd/test_phase.rs`.
The watchdog appends through a second descriptor, bypassing the mutex. `O_APPEND` positions each syscall, but a short write followed by a retry can interleave. Publishing precedes `Opened` and `Closed` precedes clearing the live flag, so a watchdog record can land outside the frame. `dup2` reuse clears close-on-exec. The reader treats the last `Closed` as closure, ignores `Opened.run_id` and `LaneStarted`, and can consume records from the wrong run. `LaneFinished.passed` is only a finish marker. Read errors drop the I/O cause, poison makes opening look successful while appends no-op, and the first write error is dropped on close.
Enforcement: a run-owned single ordered writer (with a separate bounded watchdog record if needed), and a validated journal decoder.
Reported by: check-orchestration, test-runner.

## PROC-042 - Phase time, failure and run lifetime have separate owners

Sites: `src/check_cmd/watchdog.rs`, `src/check_cmd/phase.rs`, `src/check_cmd/verdict.rs`, `src/cargo_env.rs`, `src/check_cmd/report.rs`, `src/check_cmd/continuation.rs`, `src/build_purpose/environment.rs`, `src/test_cmd/run.rs`.
The active clock is global, the failure phase is a mutable local, lock activity is thread-local and the watchdog records a string. Re-entering `ScriptCheck` restarts its clock. Clearing the failure phase leaves clock and activity in place during finalisation. The journal, watchdog result, Cargo env failure cache and warning-dedup sets ("once per run") live for the process, and `reconcile_run` reads a global flag despite being described as pure. The watchdog main loop reads a mutex-backed clock that recovery can hold while rendering. Focused `test` resets the sweep clock at preparation, recheck and before iteration one, while the manual says it covers preparation through the first run.
Enforcement: phase identity and deadline in the stop itself, a run handle for per-run state, and a two-run test.
Reported by: check-orchestration, test-runner.

## PROC-043 - Permits and threads depend on the happy path

Sites: `src/check_cmd/parallel.rs`, `src/check_cmd/prepare.rs`, `src/check_cmd/concurrency.rs`.
Parallel workers call `release` after a binary, and a panic skips it, leaving another worker waiting on a condvar with no shutdown check. Hash readers have the same manual protocol. Grouped builds and background scripts use thread-creation APIs that panic on exhaustion, and background scripts start a thread per entry before convention scans. Background `join` holds the state mutex through every join and through script output, then reduces errors to a generic already-reported error.
Enforcement: RAII permits, stop-aware waits, a fallible bounded pool, and moving handles out under a short lock.
Reported by: check-orchestration, convention-gates.

## PROC-044 - Deadline semantics hide different policies behind equal numbers

Sites: `src/check_cmd/binaries.rs`, `src/check_cmd/nextest_lane.rs`, `src/output/child.rs`, `src/test_runner/procgroup.rs`, `src/test_cmd/plan.rs`, `src/test_runner/deadline.rs`, `src/check_cmd/conventions.rs`, `docs/commands/check.hygiene.md`.
Direct listings use the 20 s test ceiling as a process wall deadline, while test bodies use it as a lifecycle clock. A Cargo prebuild refreshes its five-minute idle timer on any output. Serial scripts get the remaining budget minus ten seconds with a one-second floor that can exceed what's left, named checks get a fresh allowance per entry, and background ones each get the full ceiling. A process that reported its final suite and then wedges is still on the 20 s no-completion clock during teardown, where the guide describes a five-minute idle window (`observe_suite_end` only clears `in_suite`).
Enforcement: typed absolute deadlines with refresh events and an injected clock, and one scheduler for script entries.
Reported by: check-orchestration, test-runner, convention-gates.

## PROC-045 - Failures and evidence degrade quietly

Sites: `src/check_cmd/binary_timings.rs`, `src/check_cmd/prepare.rs`, `src/check_cmd/coverage.rs`, `src/check_cmd/phase.rs`, `src/test_cmd/run.rs`, `src/test_runner/procgroup.rs`, `src/test_cmd/focused.rs`, `src/check_cmd/diagnostics.rs`.
Timing-store reads collapse missing, unreadable and invalid TOML into an empty history, and writes and cleanup can be silent. The fixed `.partial` name depends on external serialisation. An auxiliary existence listing discards failures and marks liveness unknown. Advisory liveness skips nextest, unattributable lanes and lanes with libtest flags, and a green run doesn't say which filters went unjudged. Plan persistence failure prints as an error even on a passing run. Focused accounting-open failure errors and continues. Drain cutoff warns without harness, stream or iteration. Discovery can print a second generic failure before the real one. Grouped diagnostic errors lose sweep and resolution identity.
Enforcement: typed evidence states with lane, reason and effect, and level chosen by effect.
Reported by: check-orchestration, test-runner.

## PROC-046 - Accounting storage has count limits and no byte limits

Sites: `src/check_cmd/accounting/journal.rs`, `src/check_cmd/continuation.rs`, `src/check_cmd/nextest_lane.rs`, `src/run_log.rs`.
The journal appends every observation and then reads the whole file into memory. Plans serialise whole inventories, and continuation clones and renders unresolved executions. Ten-directory retention keeps an extra directory when the opening run sorts outside the newest ten (after a clock rollback), and entry errors are skipped. Accounting ids use wall ms and PID with zero for pre-epoch and reuse an existing dir.
Enforcement: per-run and aggregate byte quotas, streaming reconciliation, and a create-exclusive primitive with bounded attempts.
Reported by: check-orchestration, test-runner.

## PROC-047 - Audit uses the writable command setup

Sites: `src/main_parts/bootstrap.rs`, `src/tree_policy.rs`, `docs/commands/check.md`, `src/cli/schema.rs`, `src/check_cmd/check_plan.rs`.
`check --audit` opens a run log and takes the lock before branching, so tree policy can move the toolchain pin and swap the Cargo config. The CLI says audit runs nothing. A Git walk or presence error exits through `?` before package-selector and stale-allow findings print, contrary to its per-branch policy, and it probes paths no selector needs.
Enforcement: model tree policy in memory for audit, with typed live, dead and couldn't-inspect results per branch.
Reported by: check-orchestration, config-cli-startup, convention-gates.

## Test runner

## PROC-048 - Discovery and execution decode the same protocol differently

Sites: `src/test_cmd/focused.rs`, `src/check_cmd/binaries.rs`, `src/test_runner/libtest_json.rs`, `src/test_runner/outcome.rs`, `src/test_runner/deadline.rs`, `src/test_cmd/display.rs`, `src/test_runner/output.rs`, `reference/check-internals.md`.
Focused discovery requires JSON open and close records, UTF-8 and agreed counts, but accepts duplicate names and treats missing `ignore` as false. Check's `binary_list` uses a terse tally, lossy UTF-8 and a separate benchmark parser. Both use `run_listing` but order timeout against unsettled output differently. In execution, malformed counters become zero, nameless test events disappear, unknown records become output, and the reducer ignores the announced `test_count`. Test output shares the channel that drives clock transitions, so a new name or apparent suite boundary in output can change a kill decision. Cargo's `Finished`, `Running`, failure and backtrace strings are interpreted separately for filtering, timing, build failure and crash reports.
Enforcement: one typed discovery decoder and one validated event stream driving a single state machine, with a version-tested text adapter for the remaining phrases.
Reported by: test-runner.

## PROC-049 - Focused rechecks leave execution ownership split

Sites: `src/test_cmd/plan.rs`, `src/test_cmd/run.rs`, `reference/check-internals.md`.
Focused `test` builds and discovers every sweep, then rebuilds each before running. The saved envelope survives a recheck that compares artifacts and fingerprints but not the full launch config.
Enforcement: a run-owned immutable launch closure (executables, support binaries, runtime libs, environment) held for the iteration.
Reported by: test-runner.

## PROC-050 - The harness shim has host and locking assumptions

Sites: `src/test_runner/harness_shim.rs`, `src/maintenance.rs`.
Socket binding under `~/.brokkr/harness` doesn't pre-check address length, so a long home fails at setup. Non-UTF-8 socket paths convert lossily, and executable names cross a raw-byte sender to a UTF-8 receiver. A kernel without pidfds passes the `/proc/self/exe` preflight and fails at handshake. One acceptor waits for the hello, the build gate and a full-file hash before accepting another peer, and reads the hello before checking parentage. The five-second timeout applies per read. `accept_one` holds the drain and harness-list mutexes across pipe creation and send, while the watchdog and shutdown need those lists. Accept errors end the acceptor silently. A pidfd failure becomes "runner exited during handshake", and a poll error reads as alive. The permissive warning claims the shared stream even when sending the verdict failed. A unit test creates real `~/.brokkr/harness` entries. Setup failures before `Session` exists leave partial directories for maintenance, which recognises names separately from the generator.
Enforcement: probe capabilities before lane selection, a short private runtime dir, an absolute transaction deadline with no IPC or hashing under locks, preserved failure causes, and a state-root seam for tests.
Reported by: test-runner.

## PROC-051 - The scratch allocator and hang snapshots lack ownership proof

Sites: `src/test_scratch.rs`, `src/test_runner/hung.rs`.
`target/test-tmp` uniqueness covers `(module, name)` within one process, so two processes testing one checkout share and can delete each other's directory. Path components aren't constrained, and `scratch_path` discards removal errors. Hang snapshots can reuse a seconds/PID/name path and overwrite `evidence.txt`. The pruner deletes every directory under the root after an mtime sort, whoever made it, and discards errors. The process-wide snapshot mutex is held across enumeration, deletion and writing.
Enforcement: per-invocation exclusive allocation under project scratch state, and a snapshot store that claims names exclusively and prunes only recognised entries.
Reported by: test-runner.

## PROC-052 - The test-code classifier is a second module and cfg interpreter

Sites: `src/test_code/graph.rs`, `src/test_code/cfg.rs`, `src/test_code/facts.rs`, `src/textlint.rs`.
`Reader` injects text, but module existence, canonicalisation and ambiguity checks hit the live filesystem, so reachability and masks can come from different observations. Any canonicalisation error counts as absent, test-context read failures vanish, and standalone read or parse errors give empty masks, which can leave an `only` rule with nothing selected and no cause. It walks literal `#[path]` and `include!` edges outside the checkout, disables bare `#[test]` proofs globally on any shadowing, ignores `cfg_attr`-introduced cfg for production absence, and has no visit, byte or nesting budget.
Enforcement: one immutable source graph through an injected resolver with absent, unreadable, unsupported and parsed states, bounded traversal, and differential fixtures against rustc.
Reported by: test-runner, convention-gates.

## PROC-053 - Statically redundant branches in the runner

Sites: `src/test_runner/deadline.rs`, `src/test_cmd/verdict.rs`, `src/test_cmd/focused.rs`, `src/test_cmd/plan.rs`.
`Attribution::Suspect { stale: true }` can't happen through the only production insertion, which sets `ever_observed` at once under the same mutex. `disagrees_with_discovery` has an `Incomplete` branch its caller guards out. `focused::matches` has an exact branch that every caller passes `false` to. `resolve_debug` is test-only. Whole-suite and name matching each spell substring matching.
Enforcement: remove unreachable variants and make the invalid states unrepresentable.
Reported by: test-runner.

## Convention gates

## PROC-054 - Gate inputs come from different observations of the tree

Sites: `src/scope.rs`, `src/check_cmd/phase.rs`, `src/gremlins.rs`, `src/git/tree_state.rs`.
Planning reads Git status, `TreeSnapshot` lists files and reads status again, presence is uncached and bytes are read lazily, with canonicalisation on each read or write. An editor or retargeted symlink can change what later gates see, and a changed tree can be scanned under an earlier prose-only decision. Canonical keys leave hard links as separate entries. The cache holds one mutex over blocking reads and writes, accepts absolute paths, follows symlinks and retains everything with no byte budget. Markdown can be compiled into rustdoc or man pages, which the prose shortcut ignores.
Enforcement: one invocation inventory with a content-consistency contract, per-file loading with bounded storage, and blocking I/O outside the cache lock.
Reported by: convention-gates, config-cli-startup.

## PROC-055 - Gremlins scanner and fixer keep separate policy and write broadly

Sites: `src/gremlins.rs`, `src/scope.rs`, `src/config_parts/parser.rs`, `src/check_cmd/conventions.rs`, `docs/brokkr.toml.conventions.md`.
Built-in names and replacements list the same scalars independently, each with ASCII classification. The fixer follows symlinks to their targets, including outside the tree, and truncates the resolved file in place, and a changed link can redirect a write after the read. A later failure discards summaries of files already rewritten. Repair strips unrelated trailing whitespace on edited lines, inserts LF in CRLF files, and can rewrite a UTF-8 binary picked up by a broad include. The failure hint recommends the fixer for invalid bytes and config-only bans it can't fix. Gremlins can emit a finding per byte. The symlink behaviour is documented, so changing it needs approval. The LF and CR ban defect is in `bugs.md`.
Enforcement: one classification table with report, replace and scan-only outcomes, a verified regular worktree destination with staged replacement, and encoding and newline handling in the scan result.
Reported by: convention-gates, config-cli-startup.

## PROC-056 - Selector policy has several owners

Sites: `src/manifest.rs`, `src/check_cmd/conventions.rs`, `src/header.rs`, `src/globs.rs`, `src/check_cmd/check_plan.rs`.
The default `**/Cargo.toml` is constructed in manifest validation, scanning and audit. Header selection compiles at load and again in the scanner and audit, and expands the header separately for matching and reporting. Gremlin exclude matching is in the config type and in audit. Audit wording says "tracked" for untracked files. `globs` validates syntax but leaves the project-relative rule to chance. Empty selectors pass silently for header and manifest, and only textlint offers a liveness requirement.
Enforcement: a typed selector with defaults, compiled matchers and provenance, shared by runtime and audit.
Reported by: convention-gates.

## PROC-057 - Textlint has three matching paths with overlapping policy

Sites: `src/textlint.rs`, `src/lex.rs`, `src/config_parts/schema.rs`, `src/config_parts/parser.rs`, `docs/commands/check.md`.
Line and joined-import matches rescan a prefix for positional preconditions, though the module says they don't. Joined imports apply `except` to reconstructed text and can consume lines past `skip_after`, while multiline stops at the cut. The test-code path checks a lowercase `.rs`, but masking and import reconstruction run on any selected text, Markdown included. The lexer sees `use` inside macro input, and `in_toml_section` cuts at the first `#`. Import validation uses one sample (`use a::b;`) as proof that a rule can never fire, and an empty `allow_marker` suppresses every match. `scanned` counts files before any rule admits a position. `must_scan` can admit a region line with no text and says "not a tracked file" for untracked ones. `TextlintRule` carries authored fields plus an optional compiled copy that runtime trusts. Preset and mergeable field lists are kept apart from the schema, and a malformed later preset list can be discarded. Coordinate maps are built three times. Findings, admission sets, bytes and masks are all retained, the regex iterator is quadratic in the worst case, and context windows can rescan large spans. The `dataset-defaults-have-one-owner` rule matches quoted literals in `region = "code"`, where textlint blanks literals, and points to defaults `cli/schema.rs` no longer declares. `skip_test` aliases `test_code`, `except_above` and `require_above` (and the below pair) mean the same thing, and the local config doesn't use `require_live_paths` or `must_scan`.
Enforcement: typed source spans adjudicated by one policy, language classification for files, immutable executable rules built from the authored schema, scan budgets with an incomplete verdict, and a violating fixture for every ownership rule.
Reported by: convention-gates, config-cli-startup, visual-projects.

## PROC-058 - Manifest and dependency rules use local heuristics

Sites: `src/manifest.rs`, `src/dependency_rules.rs`, `src/config_parts/parser.rs`, `src/cargo/metadata.rs`, `src/deps/publish_cycle.rs`, `docs/brokkr.toml.conventions.md`.
Version alignment keys on the dependency key, not the effective package, takes the first versioned declaration, ignores other kinds and targets, and `version_key` takes the first numeric fragment (`>=1.2, <2` gives `1.2`, `>=1, <2` is unclassified). `[lints] workspace` and bin/example flags inspect explicit tables, so auto-discovered targets escape. Machete `ignored` values of the wrong shape are skipped. An empty `[manifest]` still parses files and can fail, though documented as inert. Polarity is checked in both parser and phase. Unknown wire kinds are refused by dependency rules and skipped by publication cycles. A mixed wildcard `from` ignores the other names. Repeated names repeat findings, `except` can remove every candidate silently, the green count is the workspace package count, selector errors omit the rule label, and a later bad selector discards earlier violations. The first-workspace defect is in `bugs.md`.
Enforcement: Cargo-compatible requirement parsing, Cargo target facts where a rule means a target, typed polarity, and rules resolved before evaluation. Changing what existing configs accept needs approval.
Reported by: convention-gates.

## PROC-059 - Gate diagnostics are assembled per gate

Sites: `src/check_cmd/conventions.rs`, `src/check_cmd/fixture_lint/mod.rs`, `src/cargo_config_strip/mod.rs`.
Gremlins, dependency rules and publication cycles each build their finding block beside the generic `report_findings`. Fixtures have their own green line, error prefix, location format and argv joiner. Script errors add their own stream and status text. Fixture errors bypass `output::path` for primary spans. One-shot successes and failed path probes range from silent to detail to warning.
Enforcement: phase, gate identity, typed location, severity and subprocess evidence carried into one renderer with console bounds.
Reported by: convention-gates.

## PROC-060 - Repo-local gate scripts are unwired or heuristic

Sites: `scripts/check-man-citations.py`, `scripts/check-docs-source-paths.py`, `brokkr.toml`, `src/man.rs`.
`check-man-citations.py` is tracked, has no caller, runs the installed `brokkr` and knows one citation syntax, and the man tests check slugs but not citations. Nothing rules out manual use. `check-docs-source-paths.py` walks `src/` and `docs/` itself with its own extension rules, a fixed allowlist and a type-definition regex that also reads comments and strings, and its sentinel is authored separately in `brokkr.toml`.
Enforcement: citation checks through the in-tree topic and section resolver, and the common inventory plus a Rust syntax view for the source-path rule.
Reported by: convention-gates.

## Serve

## PROC-061 - "What is live?" has three answers in serve

Sites: `src/serve.rs`, `src/serve/registry.rs`, `src/serve/carriers.rs`.
Listing calls `outstanding`, which sweeps abandoned processes and retires entries, so an ordinary read can reconcile and kill. Stop reconciles every generation before selecting one, and lock reporting only looks. Fresh-hold warnings use raw entries, so they can warn about retired generations and skip errors. Readers take the same flock that establishes supervision, so a probe can be mistaken for a supervisor. A free record that parses badly is retired as an interrupted publication. Intent accepts arbitrary paths, a loosely parsed boolean and a token independent of the filename, and retirement removes files via the stored path. Publication and retirement hold the registry flock through `sync_data` or recursive removal, and publish discards its rollback error. Refusal hints give a bare name, which is ambiguous across roots, and a short id that can collide. `.lock` is repeated, and phase strings and record fields are hand-written and parsed. The persisted `with` field is written and parsed but unused by launch.
Enforcement: one typed registry state machine with an atomic intent commit, an explicit corruption state, a separate ownership handle, and paths derived from verified roots.
Reported by: worktree-clean-git-deps.

## PROC-062 - Serve process ownership depends on cooperating children

Sites: `src/serve/registry.rs`, `src/process/identity.rs`, `src/serve/carriers.rs`, `src/serve.rs`.
A child that leaves the process group and namespace, or drops the token across exec, outlives the supervisor. A different boot id is treated as historical even on shared home storage. Missing or denied pidfds can leave stop waiting its full budget while open and signal errors vanish. See also `hygiene-host.md` (procfs facts).
Enforcement: a host-scoped identity provider with typed uncertainty, and a delegated cgroup or equivalent container per generation where the guarantee must hold.
Reported by: worktree-clean-git-deps.

## PROC-063 - Serve lifecycle budgets and its log

Sites: `src/main_parts/bootstrap.rs`, `src/serve.rs`, `src/serve/supervisor.rs`, `src/serve/registry.rs`, `docs/commands/serve.md`.
Start validates the name and host prerequisites only after building and probing, and can stop an old instance before later setup fails. Stop combines several waits around a blocking registry, while the docs promise grace plus margin. A cleanup failure after interrupted startup is only warned. Both server streams append forever. Readiness rereads the whole log every 100 ms under the global lock and treats read failure as not-ready. The failure tail loads the whole file as UTF-8 before keeping 20 lines, so one bad byte erases it. The supervisor has a handwritten prefix and ending formatter, and a failed `Ended` write is discarded. Crashes leave `.state.tmp` files. Generations under renamed or abandoned roots have no global budget. The saved environment points library paths at Cargo targets that later builds or eviction can change. The grace defect is in `bugs.md`.
Enforcement: a validated request with one monotonic deadline per operation, a separate readiness channel or bounded stream matching, a bounded byte tail, a typed exit record, and frozen runtime files or leases.
Reported by: worktree-clean-git-deps, launch-and-install, config-cli-startup.
