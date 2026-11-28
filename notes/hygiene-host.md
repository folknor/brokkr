# Hygiene: host, environment, filesystem and output

Findings from the hygiene hunt: values with more than one owner, and assumptions about the host. Sibling files: `hygiene-cargo.md`, `hygiene-process.md`, `hygiene-stores.md`. Defects are in `bugs.md`. Nothing here has been verified.

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

## HOST-001 - Cargo home has two resolvers

Sites: `src/user_dirs.rs` (`cargo_home`), `src/rustflags.rs` (`cargo_home`), `src/guard.rs`, `src/build_info.rs`, `src/crit_cmd/stamp.rs`.
Both try nonempty `CARGO_HOME`, then `HOME/.cargo`, then passwd. `user_dirs` anchors a relative `CARGO_HOME` to the process cwd and returns a relative `HOME` fallback as spelled. `rustflags` anchors both to Cargo's child cwd and returns a typed failure when neither yields a home. So guard enrolment, part of crit's provenance and the actual launch can name different config trees. Install root detection uses the launch resolver but walks paths without the canonicalisation ordinary launch discovery does.
Enforcement: one pure resolver over an explicit cwd, environment and account lookup. Table tests for absent, empty, relative, symlinked and non-UTF-8 inputs. A text rule against reading these variables outside the resolver.
Reported by: test-runner, cargo-integration, launch-and-install, lock, config-cli-startup, host-fs-output, worktree-clean-git-deps.

## HOST-002 - HOME has several meanings in one invocation

Sites: `src/user_dirs/paths.rs`, `src/user_dirs.rs`, `src/history.rs` (redaction), `src/bin/results_server/deploy.rs`, `src/config_parts/user.rs` (`BROKKR_USER_CONFIG`).
The shared lock and state resolver (also compiled into the rustc guard) accepts any nonempty `HOME`, relative included, with no passwd fallback. So the cross-project lock, queue, serve registry, worktree container, reservation store, history and local bench store can resolve under the invocation directory and split by cwd. XDG overrides must be absolute, but their fallback inherits the relative home. History redaction recognises only an absolute UTF-8 home. Deploy requires an absolute home and uses a literal `~/.config`, ignoring `XDG_CONFIG_HOME`. `BROKKR_USER_CONFIG` treats empty as disabled and accepts a relative path. A holder and its Cargo child in different directories can open different locks.
Enforcement: resolve an absolute account context once and give each intentional policy a name. Run the same environment matrix across all binaries. Changing the live lock path needs coordinating with the guard.
Reported by: lock, process-and-reservations, results-server-and-stores, config-cli-startup, host-fs-output, worktree-clean-git-deps, visual-projects, launch-and-install.

## HOST-003 - An explicit user-config path that doesn't exist is silently dropped

Sites: `src/config_parts/user.rs`, `src/env.rs`.
Startup discards the attempted path on `NotFound`, including an explicit `BROKKR_USER_CONFIG`, so a typo drops the user's gates. `env` calls the resolver again to display absence, and its tests compile out that branch's environment lookup.
Enforcement: keep a startup record (disabled, absent default, absent explicit, loaded) and report it from `env`. A warning stays inside the config contract; a refusal would need approval.
Reported by: host-fs-output.

## HOST-004 - Hostname observation is hidden in the config parser

Sites: `src/config_parts/parser.rs`, `src/env.rs`, `src/bench/view.rs`, `../elivagar/crates/devkit/src/config.rs`, `../pbfhogg/crates/devkit/src/datasets.rs`.
`gethostname` is cached for the whole process, failure included, with a fixed 256-byte buffer, no errno, and UTF-8 required. Host features silently become empty on failure, context bootstrap propagates the failure, and an unmatched host falls back silently in ordinary commands but warns in `env`. Elivagar reads `/proc/sys/kernel/hostname` instead, and pbfhogg requires a known host where brokkr defaults to an empty registry. `HostConfig.features` says "all builds", but the manual limits which build paths inherit features.
Enforcement: a host adapter that reports the selected table and the reason for any fallback. Conformance fixtures for standalone consumers.
Reported by: config-cli-startup, host-fs-output, visual-projects.

## HOST-005 - Host data paths are resolved by three readers

Sites: `src/config_parts/parser.rs`, `src/bench/view.rs`, `src/bench/runner/mod.rs`, `../elivagar/crates/devkit/src/config.rs`.
The typed resolver and the bench Lua view each pick the host section and spell `data` and `data/scratch`. The view and elivagar each default `data/tilegen`. The runner lexically folds the scratch path before opening it while the Lua view keeps the original components, so a symlink followed by `..` can reach different directories. The view also prefers the explicit namespace on a raw table, which matters when a stored view is reconstructed outside current validation. A host `target` changes the path `env` displays while Cargo and clean use Cargo's target.
Enforcement: resolve the host and its named path roles once, with provenance, and build the Lua projection from that. A symlink fixture.
Reported by: bench, config-cli-startup, visual-projects.

## HOST-006 - Startup OS inputs are captured in several places

Sites: `src/main_parts/bootstrap.rs`, `src/project.rs`, `src/context.rs`, `src/check_cmd/watchdog.rs`, `src/run_log.rs`, `src/sidecar.rs`, `src/utf8_path.rs`.
Project detection, setup, the CLI parser, watchdog history and log headers each obtain cwd or argv. The launcher keeps OS bytes, but the FIFO path getter refuses non-UTF-8. Run-env scrubbing relies on an unsafe early environment mutation with a single-threaded ordering assumption. A recognised serve-supervisor marker with a missing or malformed descriptor returns `None` and falls into public CLI parsing, losing the protocol error. For the panic itself see `bugs.md` (non-UTF-8 argv and env).
Enforcement: capture `args_os`, the environment and cwd once into a startup context. A lint restricting ambient argv and env access to the adapter. Parse the invocation mode once.
Reported by: config-cli-startup, host-fs-output, lock.

## HOST-007 - Non-UTF-8 paths survive some paths and change meaning on others

Sites: `src/runnables.rs`, `src/passthrough_cmd.rs`, `src/build_purpose.rs`, `src/build_purpose/environment.rs`, `src/main_parts/bootstrap.rs`, `src/target_platform.rs`, `src/rustflags.rs`, `src/launch/probed.rs`, `src/bench/view.rs`.
Install renders the package dir and root through `display()` into argv. Passthrough converts its executable that way although spawn takes `OsStr`. The worktree target override goes through `display()`. `build` prints its path through `display()` as shell-consumed data, so a newline breaks the one-line contract. Compiler and wrapper selection drop non-UTF-8 values and try lower-priority inputs, while launch receipts keep bytes. Bench path projection alternates between explicit refusal and lossy spelling.
Enforcement: OS-native argv and path types through spawn. A lint against display-to-argv conversion. A data-path writer with defined framing.
Reported by: cargo-integration, launch-and-install, bench.

## HOST-008 - CPU budgets are resolved independently by each consumer

Sites: `src/cpu_topology.rs`, `src/cpu_topology/quota.rs`, `src/config_parts/check.rs`, `src/check_cmd/prepare.rs`, `src/check_cmd/nextest_lane.rs`, `src/host/cpu.rs`, `src/env.rs`.
Parallel libtest uses physical cores, affinity, cache topology and visible cgroup v2 quota. Nextest uses its own discovery. Hashing samples `available_parallelism().clamp(1, 8)` once for permits and again for workers, and a single-file hash bypasses the permits. The cache probe groups by `(package, core_id)` while `budget = "cores"` uses sibling masks. It assumes `index3` and picks the first usable domain without placing work there. Frequency and governor come from `cpu0` even if it is offline or excluded. CPU-list decoding expands ranges into a `BTreeSet` with no bound and accepts partial malformed lists. Missing sysfs degrades to logical parallelism and finally to four. Profile validation probes hardware at config load. See also `bugs.md` (CPU inventory).
Enforcement: one host topology snapshot with machine, affinity, cache and effective-budget fields, a bounded range type, and sysfs fixtures (multisocket, repeated core ids, quota-limited, sparse).
Reported by: check-orchestration, test-runner, config-cli-startup, host-fs-output.

## HOST-009 - Mount parsing has three owners

Sites: `src/cpu_topology/quota.rs`, `src/lock/cgroup.rs`, `src/host/storage.rs`.
The quota reader understands escaped mount paths and non-root cgroup mounts. The lock cgroup reader rejects both, so a container can get the quota cap but no cgroup containment. The storage parser's decoding defects are in `bugs.md`.
Enforcement: one byte-preserving mountinfo decoder with fixtures for Unicode, escapes, bind mounts and rooted cgroups.
Reported by: host-fs-output.

## HOST-010 - Storage identity and unknown space vary by caller

Sites: `src/host/storage.rs`, `src/disk_gate.rs`, `src/health.rs`, `src/bench/host_trace.rs`, `src/env.rs`.
`env` and health group on mount and device strings. The gate deduplicates on the mount string and rereads the mount table for each candidate. Storage resolves `/dev` names and follows partition parents, while host trace starts from `st_dev` and follows a sysfs parent. Failed `statvfs` becomes zero total: an explicit unavailable row in `env`, a silent admission in the gate, a silent omission in check's pressure report. `env` says low data and scratch mounts make locked commands refuse, but the gate never probes them. The 5% admission and 10% state-pressure thresholds live in different modules, and neither checks inodes, write size or quota.
Enforcement: a typed mount identity and probe result, one disk-policy owner with named thresholds, and an explicit decision on unknown.
Reported by: host-fs-output.

## HOST-011 - Memory absence becomes a factual zero

Sites: `src/host/memory.rs`, `src/bench/runner/mod.rs`, `src/bench/host_trace.rs`, `src/history_cmd.rs`.
`host::memory` returns zero when procfs or a field is missing, and bench provenance stores it as `Some(0)`. History records zero available memory when `/proc/meminfo` can't be read. Host trace reads the same file and treats missing dirty/writeback as `None`. Host-wide `MemAvailable` ignores a tighter container ceiling.
Enforcement: one decoder that keeps presence, units and source.
Reported by: host-fs-output, results-server-and-stores.

## HOST-012 - io_uring "supported" is a prediction

Sites: `src/host/kernel.rs`.
The report folds three sysctls to booleans, treats unreadable controls as clear, uses a fixed 16 MiB memlock policy and never tries ring setup. Seccomp can still make creation fail, and blocker names double as lookup keys for remedies, with a wildcard picking the user-namespace remedy.
Enforcement: an enum for observations, separated from any capability probe.
Reported by: host-fs-output.

## HOST-013 - Procfs facts are re-derived by every consumer

Sites: `src/process/mod.rs`, `src/process/table.rs`, `src/process/inflight.rs`, `src/lock/collector.rs`, `src/lock/presentation.rs`, `src/check_cmd/watchdog.rs`, `src/sidecar.rs`, `src/sidecar_fmt.rs`, `src/bench/host_trace.rs`, `src/serve/registry.rs`, `src/lock/hold_protocol.rs`.
`/proc/uptime` is parsed in the watchdog, lock collector, lock presentation and in-flight report with different precision and fallbacks. `_SC_CLK_TCK` is read again by the watchdog and sidecar rendering: 100 on failure in one, `None` in the helper, zero age in the watchdog. Sidecar normalises historical CPU ticks with the viewer's tick rate because evidence stores no source rate. Bench trace rereads the boot id and accepts any nonempty string, while process safety uses `BootId`. `ProcStat` drops a whole row on invalid-UTF-8 `comm`, maps invalid fields to absence and missing RSS to zero. Serve reimplements boot, PID-namespace and time-namespace discovery despite `HostScope::current`.
Enforcement: one procfs capability with typed boot, namespace, tick and uptime facts and explicit unknowns. Persist the tick rate with new evidence. Ban direct `_SC_CLK_TCK` outside the adapter.
Reported by: check-orchestration, lock, process-and-reservations, results-server-and-stores, host-fs-output, bench, worktree-clean-git-deps.

## HOST-014 - Clock acquisition and calendar arithmetic leak policy

Sites: `src/boot_clock.rs`, `src/output/child.rs`, `build.rs`, `src/civil_date.rs`, `src/deps/ccu.rs`, `src/bin/fstrim_helper.rs`, `src/bench/runner/mod.rs`, `src/bench/marking.rs`.
`BootInstant`, `BootElapsed` and `ChildUs` protect units, but production calls the real clocks directly. A failed boot read at run start refuses with a bare I/O error, while the supervisor's end read drops the error into `None`. A negative decoded interval becomes zero. `build.rs` repeats the civil-date algorithm in `civil_date.rs`, and `ccu` has its own date parsing that accepts impossible dates and ignores offsets. Malformed `SOURCE_DATE_EPOCH` gives `unknown`, while a non-UTF-8 one falls back to the wall clock. The fstrim helper ignores its boot-clock error, so a bad read can become a plausible duration. The runner and marking each convert Unix milliseconds, mapping pre-epoch to zero.
Enforcement: share the calendar core with the build script (it is core-only). One clock interface with paired readings. Fallible span decoding.
Reported by: launch-and-install, host-fs-output, process-and-reservations, worktree-clean-git-deps, bench.

## HOST-015 - Entropy and identifiers have separate plumbing

Sites: `src/lock_tokens.rs`, `src/db/types.rs`, `src/bin/results_server/identity.rs`, `src/install_step/transaction.rs`, `src/launch/mod.rs`, `src/test_runner/harness_shim.rs`, `src/check_cmd/accounting/journal.rs`, `src/test_runner/hung.rs`, `src/test_orphans.rs`.
Run tokens read `/dev/urandom` and lose the I/O cause. Run UUIDs open it separately. Deploy uses nonblocking `getrandom` and reports short reads through `last_os_error`. Install transactions use blocking `getrandom`. Launch names use PID, subsecond wall time and a counter. The shim combines PID, wall time and a sequence. Accounting uses milliseconds and PID with a pre-epoch fallback to zero. Hang snapshots use seconds, PID and a sanitised test name. The orphan token caches its first failure for the process, so one transient failure disables later stamping. The inert hold mints a nonce and discards it.
Enforcement: one injected entropy and id source with explicit failure policy, and exclusive creation for on-disk identities.
Reported by: test-runner, check-orchestration, launch-and-install, lock, results-server-and-stores, visual-projects.

## HOST-016 - The disk gate guesses where builds write

Sites: `src/disk_gate.rs`, `src/env.rs`, `src/worktree.rs`, `src/build.rs`, `src/config_parts/parser.rs`.
The gate probes `<build root>/target` plus a nonempty ambient `CARGO_TARGET_DIR` and skips paths it can't probe. It doesn't resolve Cargo's config target, build dir, per-request overrides or the historical build's location. `env` uses Cargo metadata, then prefers the host `target` key. So the gate can refuse over a full unused disk or admit work onto a full real destination. The worktree cut adds a separate container probe. The gate deliberately excludes project state, which can still hold unbounded logs and caches.
Enforcement: give one admission check the planned write destinations from the build plan, and report unknown probes. See also `hygiene-cargo.md` (target and build dir isolation).
Reported by: cargo-integration, lock, config-cli-startup, host-fs-output, worktree-clean-git-deps, launch-and-install.

## HOST-017 - Project state is initialised before the read-only decision

Sites: `src/main_parts/dispatch.rs`, `src/clean/layout.rs`.
`ensure_self_ignored` runs before `reads_only`. It accepts an existing `.gitignore` of any contents, follows a symlinked `.brokkr`, and creates the marker before writing its contents, so a failed write leaves a marker that later `AlreadyExists` treats as success. The warning names `.brokkr/.gitignore` without the project or worktree root. Visual queries and other read-only commands therefore write. See also `bugs.md` (dry runs and refusals have effects).
Enforcement: give mutating commands an initialised `ProjectState` capability and publish the marker atomically. Test read-only dispatch with no `.brokkr`.
Reported by: config-cli-startup, host-fs-output, visual-projects, worktree-clean-git-deps.

## HOST-018 - Atomic replacement's ownership and metadata policy is implicit

Sites: `src/atomic_write.rs`.
`Staged::stable` hands out a predictable sibling name, and its single-writer rule is only a comment. The module reopens the directory between write and publication, and drop removes a pathname rather than the inode it created. The pre-rename identity check is separated from rename by chmod and fsync. Ordinary replacement follows an existing symlink but replaces the link itself when the target is missing. Durable replacement refuses a final symlink but trusts intermediate links. Temp files use `0666` less umask until publication copies permission bits. Owner, group, ACLs and xattrs aren't carried, a failed mode read silently leaves defaults, and unsupported directory fsync is silent in the atomic tier.
Enforcement: keep the directory and staged-file capability through the transaction, make destination authority, temp permissions and durability explicit, and default to private staging.
Reported by: host-fs-output, convention-gates, visual-projects.

## HOST-019 - Single-file publication is hand-rolled in several stores

Sites: `src/bench/host_trace.rs` (cursor), `src/serve/registry.rs` (state), `src/check_cmd/binary_timings.rs`, `src/launch/helper.rs`, `src/runnables.rs` (snapshots), `src/worktree_lock.rs`.
The bench cursor and serve state do direct temp writes and renames with fixed names (`.json.tmp`, `.state.tmp`). Binary timings ignore temp-write failure and remove the temp only when rename fails. Launcher extraction and snapshots hand-roll staging. Worktree lock seeding fsyncs and then uses a separate rename path.
Enforcement: a structural rule banning open-coded publication in these modules, with each store calling the shared primitive at its required durability.
Reported by: host-fs-output, bench.

## HOST-020 - The digest cache trusts host metadata

Sites: `src/preflight/digest.rs`, `src/datasets/status.rs`.
A two-second wall-clock window gates recording, and a cache hit compares mtime, ctime, inode and size, but not device. Remounts, network metadata and coarse timestamps break the identity assumption. The stable reader checks type before opening through a different operation and checks inode only after the read, so a special file can block it. The uncached reader accepts any openable file and repeats the 64 KiB hash loop, which the results server reimplements as well. Tree hashing converts names lossily, ignores empty directories, crosses mounts, and hashes internal symlink target text rather than content. The cache loads and rewrites the whole file under a blocking flock that also covers existence probes of every entry. It prunes only missing files and keys on canonical absolute paths, which carry personal home or delivery names.
Enforcement: one descriptor-based regular-file reader, a versioned byte-path tree codec, filesystem-aware cache eligibility, and an entry and byte budget.
Reported by: bench, host-fs-output, visual-projects, results-server-and-stores.

## HOST-021 - Directory walks have no resource envelope

Sites: `src/health.rs`, `src/preflight/digest.rs`, `src/maintenance.rs`, `src/clean.rs`, `src/litehtml/cmd.rs`.
`health::usage` keeps an inode set and a work stack, follows the root symlink, and can't detect a same-device bind mount. The tree walk keeps every entry for sorting. Maintenance, cleanup's prefixed targets, the litehtml cache, library dirs, serve generations and worktree walks treat unreadable directories and entries as empty.
Enforcement: per-scan depth, work and time budgets, and an incomplete result that names the affected path.
Reported by: host-fs-output, worktree-clean-git-deps.

## HOST-022 - Output presentation has many small owners

Sites: `src/output/console.rs`, `src/history.rs`, `src/diagnostic.rs`, `src/cargo_env.rs`, `src/passthrough_cmd.rs`, `src/sidecar.rs`, `src/invalidate_cmd.rs`, `src/check_cmd/test_execution.rs`, `src/test_cmd/run.rs`, `src/test_cmd/display.rs`, `src/check_cmd/selection.rs`, `src/output/paths.rs`.
Quiet, forced and sidecar paths hand-spell padded tags while `tag_prefix` owns the rule elsewhere. History renders `83s` as `1.4m` where the shared renderer says `1m23s`. `cargo_env` has a private millisecond formatter and a hardcoded `30s`. Passthrough prints raw milliseconds, invalidate prints elapsed ms, sidecar summaries append raw `ms`, and typed provenance rounds to seconds. Producers build "failing command" lines by joining argv with spaces, losing argument boundaries, cwd and lane. Focused execution prints arguments before the JSON protocol options are added, and the direct reproduction omits the environment. Many diagnostics use `Path::display` where the contract calls for `output::path`. `output::path` is lossy, leaves relative input relative under `with_absolute_paths`, and that thread-local override doesn't restore on unwind. Short ids have several implementations, and a fixed eight-character hint can be ambiguous. The prefix lint misses `[serve]`, `[deps]`, `[wc]` and intermediate strings.
Enforcement: typed tag, severity, location, duration and command-event fields, one argv renderer (shell-quoted, redacted), and collision-aware short ids. A source rule for `Path::display` in diagnostics. Snapshot tests for layout boundaries.
Reported by: check-orchestration, test-runner, convention-gates, launch-and-install, lock, config-cli-startup, host-fs-output, results-server-and-stores, worktree-clean-git-deps, visual-projects, bench.

## HOST-023 - Output sinks bypass the channel or lack stream ownership

Sites: `src/test_cmd/display.rs`, `src/test_cmd/verdict.rs`, `src/env.rs`, `src/health.rs`, `src/results_cmd.rs`, `src/history_cmd.rs`, `src/litehtml/cmd.rs`, `src/install_step/mod.rs`, `src/serve/supervisor.rs`, `src/bin/results_server/main.rs`, `src/bin/results_server/deploy.rs`, `src/bin/rustc_guard.rs`, `src/main_parts/bootstrap.rs`.
Focused test live forwarders and the repeat buffer write directly to stdout and stderr, skipping teeing and status coordination, and repeats merge both streams into stdout. The repeat summary embeds its own `[test]` prefix. `env` and health tables use `println!` and `env` appends warnings to that stream. Results emits a notice through `println!` in its data renderer. Outline prints email previews via `print!` with no stdout reservation, so warnings interleave. Install and serve append notices outside the console policy. The results server logs with `eprintln!` and deploy with `writeln!`, each hand-spelling the process prefix with different write-error handling, and the logger can block an async request. The guard repeats its exec-failure renderer and code `127`, and its print exemption covers human stderr as well as handshake stdout. At top level, startup prints every `DevError`, including ones already printed, while check's selected mode suppresses the repeat. For stdout-reservation defects see `bugs.md` (sidecar, deps --json).
Enforcement: a data-stream mode on the command spec before any startup output, fallible injected writers with a deliberate broken-pipe policy, one diagnostic adapter per executable, and a text or structural rule reserving direct writes for console and subprocess protocols.
Reported by: test-runner, launch-and-install, lock, results-server-and-stores, config-cli-startup, host-fs-output, visual-projects.

## HOST-024 - Bounded forced output can still block the watchdog

Sites: `src/output/console.rs`, `src/check_cmd/watchdog.rs`, `src/check_cmd/accounting/journal.rs`.
`lock_patiently` bounds mutex acquisition, then does blocking file and stdio writes. The forced fallback takes the stderr lock, and the renderer takes the stdout lock even on some stderr-only paths. The watchdog appends a journal record, prints and waits up to two seconds for the process report before requesting shutdown or killing anything, and calls forced output again before its hard exit. A full pipe or wedged filesystem can delay the kill and stop the unwind backstop from starting. The per-test watchdog already kills first.
Enforcement: put kill and hard-exit timers on a path independent of diagnostic I/O. A supervisor test that blocks each sink.
Reported by: check-orchestration, test-runner, host-fs-output.

## HOST-025 - Console and run-log state depend on call order

Sites: `src/output/console.rs`, `src/run_log.rs`, `src/lock/hold_protocol.rs`, `src/context.rs`, `src/cli/visibility.rs`, `src/main_parts/dispatch.rs`.
Quiet, stream reservation, status enablement, renderer, log file and log reservation are separate globals. `RunLog` infers nesting from a process-global `Option` and holds the reservation mutex across directory I/O, pruning and diagnostics. Log writes happen before renderer acquisition under a separate lock, so disk and screen order can differ. `detail_or_console` can see a log just before it closes. Worktree narration checks quiet before sending, so quiet removes it from the log too. `check`, `test` and `crit` reserve their own logs although dispatch already does. The `measure` log kind comment describes retired modes.
Enforcement: explicit session and stream ownership with a structured event fan-out. A typed `RunLogKind` with one reservation owner.
Reported by: host-fs-output, config-cli-startup.

## HOST-026 - Terminal width and control characters are assumptions

Sites: `src/output/console.rs`, `src/output/paths.rs`, `src/bench/results/table.rs`, `src/db/format/table.rs`, `src/lock/report.rs`.
Status uses a fixed 100-character cap and an ANSI clear sequence whenever stdout is a TTY, with no width or display-cell calculation, and assumes stderr shares the terminal. Persistent output has no line-length or control-character policy, so compiler output, kernel strings, marker text and paths can produce long or misleading lines. Table widths count bytes or scalars, not display cells. Lock indentation has no width bound. Byte formatting uses different binary-unit policies in the lock report and the console.
Enforcement: separate operator text, lossless data and private evidence in the types, and give the renderer width and escaping policy.
Reported by: host-fs-output, bench, results-server-and-stores, lock.

## HOST-027 - Errors lose their operation, path or cause

Sites: `src/error.rs`, `src/preflight/digest.rs`, `src/output/child.rs`, `src/scope.rs`, `src/history_cmd.rs`, `src/maintenance.rs`, `src/reservations/launch.rs`, `src/cargo/rustc.rs`, `src/cargo/metadata.rs`.
`DevError::Reported` and `CargoEnv` carry a label and rely on an earlier side effect they can't prove happened. Plain `Io` keeps errno but often arrives without filename or operation, as in digest opens and reads. `Ending::WaitFailed(String)` becomes `DevError::Spawn`, losing errno and calling a wait failure a start failure. `From<rusqlite::Error>` flattens to strings. `io_error` in reservations rebuilds an `io::Error` from kind and text. Metadata enumerates `DevError` variants to clone errors, rustc turns unfamiliar ones into `Build`, and both recreate I/O errors from kind and text. Scope maps all Git-status failures to `Unknown`. `TreeSnapshot::write_file` returns an I/O error without the path. Maintenance treats unreadable directories as empty.
Enforcement: errors that carry action, identity, cause and evidence, rendered once at the boundary. A scoped rule against stringifying a typed error before it crosses its API.
Reported by: host-fs-output, results-server-and-stores, process-and-reservations, cargo-integration, config-cli-startup.

## HOST-028 - Exit codes have several owners

Sites: `src/error.rs` (`result_exit_code`, `EXIT_INTERRUPTED`), `src/main_parts/bootstrap.rs`, `src/check_cmd/verdict.rs`, `src/test_cmd/verdict.rs`, `src/runnables.rs`, `src/visual/engine.rs`, `src/bin/rustc_guard.rs`.
Startup computes a code for history, then spells `0`, `1` and `130` again at process exit. Check verdict owns a partial code `10` and a literal failure code next to the shared `124` constant. The focused per-test expiry uses the generic failure code, while the phase watchdog uses 124. Run remaps a supervisor outcome to a number and then round-trips it through a receipt policy. The visual summary emits `ExitCode(1)`. Clap exits with its own usage semantics while a dispatch `DevError::Usage` exits 1. Raw wait status has two decoders: one yields `WaitFailed`, the other invents a code from raw bits.
Enforcement: one process verdict that drives both history and exit, and one child-status decoder. Exhaustive matches and tests for signal, timeout, partial and passthrough.
Reported by: test-runner, launch-and-install, lock, process-and-reservations, results-server-and-stores, config-cli-startup, host-fs-output, visual-projects.

## HOST-029 - The tunables inventory is partial

Sites: `reference/check-internals.md` (platform tunables index), `src/test_runner/ceilings.rs`, plus every subsystem below.
An index exists, and `ceilings.rs` asserts ordering at compile time. It omits, per hunter:
- check: the watchdog's 1 s poll, 100 ms kill sweep and 2 s report budget, 10 ms listing polls, the hash worker cap, the 40-line signal tail, 30-day timing retention, nextest's zero retries, first failure and zero grace, and renderer limits.
- test runner: 2 s drain in two owners, 100 ms pipe poll, 10 ms ticks, 50 ms waits, 5 s hello, 64 KiB receive, 30 s gate wait, 250 ms shim settle with 5 ms poll, 5 s evidence lead, 250/500 ms cutoffs, 4,096 tasks, 64 KiB per task file, eight collectors, 60 s pending grace, three reap passes, the five-test filter floor, 40-line crash tail.
- convention gates: per-phase ceilings, the serial script `-10 s` with a 1 s floor, capture caps (1 MiB script, 16 MiB plain, 128 MiB Cargo), five note lines of 240 chars, the 200-char excerpt, an 8,000-byte NUL probe after a full read, 5 s tree restore, and 2 s / 20 ms config takeover.
- cargo: `[cargo_env]` 30 s, 4 KiB and 16 KiB, `Duration::MAX` probes, the doctest floor and disk headroom, with no cumulative evaluator bound.
- lock: queue heartbeat, pending grace and join attempts, kill confirmation, collector refresh (750 ms floor against a ~500 ms client sleep and a 2 s cache-age tolerance, while the manual claims under one second), record and report limits, cgroup traversal, activity bounds, label clipping, and no size cap on the guard and status record readers.
- process: reservation wait, grace and poll, the 256-entry group table, traversal bounds (256, 64, 65,536), procfs byte budgets, and unbounded trim and probe policies.
- bench: 3 iterations, 10,000 rounds and 100,000 slots, 10 ms Lua stop poll, 100 ms host sampling, 30 s report-stop grace, 1 MiB previews, 64 KiB and 16 MiB stderr tails, 16 MiB and 128 MiB reports, 1 GiB scan and 8 MiB warm windows, 2 s borrow release, 5 s SQLite busy, and display caps (40-char cells, six config changes, 2,000-char stderr).
- results server: 64 KiB header buffer, 1 KiB reservation unit, 100 ms accept retry, the host-sized Tokio pool, deploy's 300 s and 10 s budgets with 100 ms to 2 s backoff, and the client's 10 s, 330 s, 3 s and 10 s.
- sidecar: 1 MiB pipe request, 4 and 16 MiB drains, the call census, 1 s clock tolerance, 1 s suspend notice, 10 ms namespace retry.
- worktree, clean and serve: `worktree_keep`, 5% disk floor, 1,024-byte LFS cutoff, 10-path warning cap, ccu's 120 s, 240 and 730 days, and serve's 15 s, 5 s, 10 s and 3 s waits, 50 and 100 ms polls, 64-byte names, 4-char stop prefix and 20-line tail.
- visual: two separate 600 s installs, 30 s navigation, 10 s HTTP inactivity, five redirects, 20 MiB responses, a 128 MiB / 512-entry image cache, the 600 px viewport, fallback sizes, a 3 px tracking rule, the Chrome 126 user agent, and outline widths.
- CLI defaults: results 20, history 25, outline depth 4, wc 800 and serve 60/5 are repeated in help text.
Some couplings are unchecked: the client's 330 s attempt against a 20 s read window, egress left out of the server's phase assertion, and the 2^53 exact-integer bound plus the 16 MiB and depth-64 caps spelled across layers in bench.
Enforcement: typed policy catalogues per owner (purpose, unit, default, override) that generate the index and expose effective values. Inject clocks and sleepers. A source rule against new policy literals at call sites. Assert the relationships between coupled values.
Reported by: all thirteen scopes.

## HOST-030 - Durable references and help text have drifted

Sites, each one claim:
- `docs/brokkr.toml.conventions.md`: says an overlapping gremlins `allow` and `ban` is a load error, but the parser accepts the overlap and `allow` wins. The check guide says emoji are removed, but the fixer transliterates some. The rustdoc lint cost note (forces a test rebuild) is obsolete.
- `docs/brokkr.toml.md`: says an invalid `results_server` fails every command, but config-independent verbs ignore it and `--version` exits 0. Says any set `CARGO_INSTALL_ROOT` counts, but the code ignores an empty one. Names brokkr as the dataset registration writer, but `pbfhogg-devkit` is.
- `docs/commands/run.md`: says preflight precedes Cargo and the receipt comes first, but the foreground source builds first. Says all three commands resolve profile through one owner, but there are two implementations plus passthrough's own policy.
- `notes/bin-redesign.md`: claims one `BuildRecipe` across launch forms and mixes three implementation generations.
- `reference/check-internals.md`: calls `BROKKR_EXEC` reporting-only, but hard kill uses it to pick members of leaderless groups. Its tunables index points to a stdout-to-file runner that no longer exists. The guard docs describe refusal at `-vV` in some places and query admission in others.
- `reference/measure-harness.md`: describes hotpath reports as launch-owned files, but they now live under the iteration's `profile/`. Says neither typed file is written, but migrations and deletes write them.
- `reference/output.md`: names `raw_stderr`, which has no implementation.
- `docs/commands/crit.md`: gives `.brokkr/bench/` as `CRITERION_HOME`, but the child gets `.brokkr/bench/criterion`.
- `docs/commands/check.hygiene.md`: says join order where the reference defines arrival as ticket creation.
- `src/cli/schema.rs` help: clean calls worktrees sibling directories, serve mentions `sync --bench`, `--human` says `--stat` is always JSONL, and typed-history help describes retired `--bench`, `--hotpath`, `--alloc` and `--force`. wc 800 and serve 60/5 are repeated beside constants.
- `src/target_platform.rs` module doc overstates which probes use the configured compiler. The `src/launch/runner.rs` header points to a `TargetFacts::host` account. The `src/stray.rs` comment still says only selected PIDs are signalled. The `src/target_namespace.rs` comment calls retired `verify` output current. `src/textlint.rs` claims no per-match prefix rescan, but one exists. `src/test_runner/libtest_json.rs` says lifecycle events no longer decide kills, but the watchdog uses them.
Enforcement: generate defaults and status tables from their owners. Script checks for command examples and externally visible sequencing.
Reported by: convention-gates, test-runner, cargo-integration, launch-and-install, lock, process-and-reservations, results-server-and-stores, config-cli-startup, host-fs-output, worktree-clean-git-deps, visual-projects.

## HOST-031 - Production-dead output and config paths

Sites: `src/output/console.rs`, `reference/output.md`, `src/config_parts/schema.rs`, `src/config_parts/parser.rs`, `src/cli/visibility.rs`, `src/main_parts/dispatch.rs`.
`QUIET` starts false and its only setter is compiled for tests, yet production branches consult it. `hotpath_msg`, `sidecar_forced` and the raw stdout replay wrappers are test-only. `LockPolicy::NonBlocking` has no producer. `ProfileDef.description` has no consumer. `verify_reference_budget` is parsed, counts toward host nonemptiness and can overflow host width, though the verify command is retired. Hidden `clean --archives` and `--keep` only lead to a refusal. Live compatibility work still happens through `[clippy]`, `skip_test`, `skip_phases = ["install_feature"]`, `--variant`, retired project tables, host `port`, `output` and `tilegen`.
Enforcement: reachability checks for internal variants, and accepted compatibility spellings marked in one schema layer. Removing any accepted key needs config-contract approval.
Reported by: host-fs-output, config-cli-startup, worktree-clean-git-deps.

## HOST-032 - The state inventory owns its own copy of retention

Sites: `src/health.rs`, `src/clean/layout.rs`, `src/check_cmd/accounting/journal.rs`, `src/test_runner/hung.rs`, `src/run_log.rs`, `docs/commands/clean.md`.
`health` spells the retention counts for logs, accounting and hung snapshots in operator strings while the writers own the limits. It omits the hash cache, bench iteration state and serve generations, which the layout names. Run logs keep ten per kind with no byte limit, old `bench-` logs fall outside every current kind, and per-process atomic temps accumulate after death.
Enforcement: every store registers its storage class, retention and visibility, and health derives from those registrations.
Reported by: host-fs-output.

## HOST-033 - env tool probes conflate failure modes

Sites: `src/env.rs`, `src/output/child.rs`, `src/git.rs`, `src/cli/visibility.rs`.
`env` runs `--version` on the inherited PATH and cwd and assumes the first stdout line has a digit-leading token. Spawn failure, nonzero exit and incomplete capture all show as `not found`. The command-output adapter has no wall limit, so a wrapper can hang `env`. Cargo metadata is a prerequisite even for CPU and memory facts. The Git revision reimplements the Git reader's `rev-parse HEAD`. The tool catalogue omits pnpm for litehtml.
Enforcement: bounded injected probes that report missing, failed, timed out and unrecognised separately.
Reported by: host-fs-output, visual-projects.

## HOST-034 - Linux and kernel capabilities are discovered at use

Sites: `src/shutdown.rs`, `src/serve.rs`, `src/cargo_config_strip/transaction.rs`, `src/install_step/transaction.rs`, `src/bench/cache.rs`, `src/test_runner/harness_shim.rs`, `src/launch/helper_main.rs`.
Startup silently accepts a `setpgid` failure although group-wide cleanup depends on isolation. Bootstrap treats every `/dev/tty` open failure as absence. Serve needs boot, PID-namespace and time-namespace identity and refuses when the namespace files are missing. Tree transactions and copy install need `renameat2` exchange and no-replace plus directory fsync. Bench cache operations need `faccessat2` with descriptor flags plus working `mmap` and `mincore`. The shim needs procfs, same-UID credentials and pidfds. The launch helper needs `/proc/self/exe`. Each fails at the operation rather than at admission.
Enforcement: capture each as a separate capability result at startup, distinguishing absence from a failed probe. Tests can enforce fallback and refusal policy, though not what a given kernel exposes.
Reported by: config-cli-startup, process-and-reservations, launch-and-install, bench, test-runner, cargo-integration.
