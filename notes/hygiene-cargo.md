# Hygiene: Cargo, build, launch, install, worktrees, cleanup, deps and crit

Findings from the hygiene hunt: values with more than one owner, and assumptions about the host. Sibling files: `hygiene-host.md`, `hygiene-process.md`, `hygiene-stores.md`. Defects are in `bugs.md`. Nothing here has been verified.

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

Several hunters proposed the same rewrite: a typed Cargo invocation that resolves executable, physical cwd, environment, arguments, toolchain and output directories once, and is carried through preflight, execution, provenance and diagnostics. Most entries below are symptoms of that value not existing.

## CARGO-001 - Three compiler queries describe different compilers

Sites: `src/cargo/rustc.rs`, `src/target_platform.rs`, `src/rustflags.rs`, `src/check_cmd/test_execution.rs`, `src/test_runner/run.rs`, `src/check_cmd/selection.rs`, `src/deps/mod.rs`, `src/crit_cmd/stamp.rs`.
The cached plain `rustc -vV` runs `rustc` from the project dir. Target templates, target cfg and libdir use the selected `RUSTC` / `build.rustc` but skip wrappers and Cargo `[env]`. Launch preflight uses `CargoProbe` with both wrapper levels. So `{host_arch}`, runner and rustflags table matching, the direct loader path, the deps host graph and crit's stamp can each see a different host. `CargoLaunchConfig` gets its triple from the root-only probe, and the serial shim's fallback check and its installation use the ambient-cwd helper. `Platform::Host` mixes the brokkr binary's OS and arch with the probed triple's vendor and env. There are three `-vV` constructions in the probe owner. `RustcInfo` accepts empty or repeated fields, `TargetCfg` accepts any nonempty line as a fact, and failures flatten to `Build` text. Cross-target readiness checks a fixed `$sysroot/lib/rustlib/<triple>/lib`, and the Cargo version parser accepts only major `1`.
Enforcement: pass the selected compiler command and environment as a value, with a separately named ambient query where that's really the question. A wrapper fixture that changes the answer, and strict protocol decoders.
Reported by: check-orchestration, test-runner, cargo-integration, launch-and-install, worktree-clean-git-deps.

## CARGO-002 - CARGO_BUILD_TARGET has several resolution rules

Sites: `src/rustflags.rs`, `src/build.rs`, `src/launch/runner.rs`, `src/test_runner/run.rs`.
Launch admission treats presence, even empty or non-UTF-8, as an ambient target and refuses direct execution. The rustflags sink parses a comma-separated string and falls through to config or host when nothing is usable. `--no-build` documents Cargo's whitespace splitting, config-array merging and canonical JSON-target stems, while rustflags placement takes a lexical stem. The runner's own argv scan takes the first `--target`.
Enforcement: one target value with absent, configured, invalid and unresolved states. A precedence matrix with Cargo parity fixtures.
Reported by: test-runner, cargo-integration.

## CARGO-003 - Cargo config is read through several views

Sites: `src/rustflags.rs`, `src/cargo_config_strip/transaction.rs`, `src/cargo_config_strip/mod.rs`, `src/cargo_env.rs`, `src/guard.rs`, `src/crit_cmd/mod.rs`, `src/launch/runner.rs`, `src/git/tree_state.rs`.
Launch reconstruction walks ancestors, home and includes. Guard enrolment edits one home file. `[cargo_env]` parses `--config` arguments itself. The two config basenames are spelled in discovery, strip recovery, guard and crit. Launch preflight reads the files, calls `CargoLaunchConfig::discover` to read them again, and may read a third time for rustflags, so a concurrent edit gives one decision from several versions. The reader models Cargo 1.97 include normalisation and 1.98 `[env] relative` provenance, and its own comments note divergence on 1.94-1.96. The legacy probe opens `config` before checking it's a regular file (a FIFO blocks), modern selection uses `.exists()`, and includes have no byte, count or depth bound.
Enforcement: one immutable config snapshot per invocation, a bounded discovery interface with typed inspection failures, and supported-version fixtures.
Reported by: cargo-integration, launch-and-install, lock, config-cli-startup, convention-gates.

## CARGO-004 - Rustflags resolver, injector and explanation are separate

Sites: `src/rustflags.rs`, `src/config_parts/check.rs`.
`effective_rustflags` reconstructs tokens, `triple_source_live` decides separately whether a layer wins, and `compose` encodes the environment separately. An unknown matching table can make reconstruction give up while placement proves the layer live. The ordering defect is in `bugs.md`.
Enforcement: one placement result with tokens, order, provenance and uncertainty, and the operator notice derived from the rendered invocation.
Reported by: cargo-integration.

## CARGO-005 - Rustc argument grammar is implemented three times

Sites: `src/rustflags.rs`, `src/cargo_config_strip/matching.rs`, `src/bin/rustc_guard.rs`.
Cfg extraction keeps an operand-taking list. Strip groups `-C` and `-Z` and keeps operand-less lists. The guard has a conservative probe classifier. Strip matching is confined to one physical value, re-tokenises edited strings with `split_whitespace()` and joins with single spaces, so an operand relationship formed by merging layers is invisible. Its "removed" message records a physical edit even when an env source shadows it.
Enforcement: a shared argument grammar where the boundary permits, an argv corpus test, and a text rule against new standalone option tables.
Reported by: convention-gates, cargo-integration, lock.

## CARGO-006 - Cargo [env] has two applicators

Sites: `src/target_platform.rs` (`CargoProbe`), `src/check_cmd/direct_runtime.rs`.
Both implement forced values and hole filling. The direct runtime builds an overlay and rereads inherited `LD_LIBRARY_PATH` for each binary's envelope. It says Cargo-owned values apply last, then appends build-script variables that can overwrite them.
Enforcement: one environment builder that records inherited, forced, generated and removed values, tested against a Cargo-mediated launch.
Reported by: cargo-integration.

## CARGO-007 - The direct launch envelope reimplements Cargo's

Sites: `src/check_cmd/direct_runtime.rs`, `src/check_cmd/test_phase.rs`.
It reconstructs `[env]`, package and build-script variables, `CARGO_BIN_EXE_*`, loader paths and `CARGO`. Package metadata is loaded with no overrides while the rest uses the sweep env. `CARGO` comes from a PATH search, rustup proxy detection by device and inode, and a silent fallback to the hit or bare `cargo`. `LD_LIBRARY_PATH` reimplements last-explicit-wins with a UTF-8-only fallback and lexical containment, and deduplicates an inherited prefix. Comments promise `OUT_DIR` is absent, but an inherited one is never removed. The runtime serial shim rediscovers launch config after preparation used a snapshot.
Enforcement: prefer a Cargo-produced launch envelope. Where reconstruction stays, add parity fixtures per supported toolchain.
Reported by: check-orchestration, test-runner, cargo-integration.

## CARGO-008 - Nextest is a second Cargo config interpreter

Sites: `src/check_cmd/nextest_lane.rs`.
Nextest canonicalises symlinked config files before reading relative values, and rejects an `[env]` entry split across files that Cargo can merge. Brokkr repeats the reserved-key removal list before rendering `ChildPolicy`. It writes a generated env file holding real values and capability material, mode 0600, which can survive an abnormal exit. Naming uses a PID and sequence loop with no attempt bound. Config removal errors are discarded. Listing gets no deadline from this caller, and the synthesised timeout truncates fractions with a one-second floor. The terminal reporter writes outside brokkr's renderer and its timeout line can omit the binary target.
Enforcement: hand the adapter the resolved env and runner, expose reserved keys from the policy owner, and add parity fixtures or early refusal for symlinks and split tables.
Reported by: check-orchestration, test-runner.

## CARGO-009 - Metadata and rustc caches block and key on partial context

Sites: `src/cargo/metadata.rs`, `src/cargo/rustc.rs`, `src/build.rs`, `src/test_code/graph.rs`, `src/runnables.rs`.
Both hold their map mutex through spawn, wait and parse, so a slow probe blocks every other key. Failures are cached for the process. Keys include paths and some explicit env pairs but omit inherited environment, effective executable, config, tree generation and phase, so results are shared across callers with different attribution and after a reseed. `project_info` applies the worktree target override, but the direct workspace queries used by textlint and dependency rules don't. Bench target discovery bypasses the cache. Locked-metadata failures always suggest `brokkr deps`, even for fixtures. Invalid JSON loses root, query and executable.
Enforcement: invocation-owned caches with per-key in-flight state, classified cacheable results, and caller context attached at the boundary. A test that blocks key A and proves key B completes.
Reported by: convention-gates, cargo-integration, launch-and-install, test-runner, worktree-clean-git-deps.

## CARGO-010 - Metadata decoding turns missing facts into valid-looking facts

Sites: `src/cargo/metadata.rs`, `src/check_cmd/fixture_lint/lock.rs`, `src/check_cmd/fixture_lint/mod.rs`.
Path-driving fields default to empty strings (a missing target dir becomes an empty `PathBuf`). Missing resolution becomes an empty graph. Unknown dependency kinds collapse to `Unknown` and lose their spelling, and unknown source prefixes become registry sources. Kind spellings are kept in display, config parser and wire parser. The fixture lock parser treats a missing package list as empty. The seed path maps every main-lock read failure to "no main lock".
Enforcement: separate workspace-only from resolved metadata at decode time, validate required paths and graph references, keep unknown wire values, and derive spellings from one enum.
Reported by: convention-gates, cargo-integration, worktree-clean-git-deps.

## CARGO-011 - Cargo JSON and diagnostics have several decoders

Sites: `src/cargo_json.rs`, `src/cargo_filter.rs`, `src/build.rs`, `src/check_cmd/binaries.rs`, `src/check_cmd/member_check.rs`, `src/rustdoc_check.rs`, `src/check_cmd/diagnostics.rs`, `src/diagnostic.rs`.
Test inventory, support builds, member-check, rustdoc stamps, fixture lint and shared diagnostics decode overlapping artifact fields separately. A malformed line is flagged only if it starts exactly `{"reason":`, and a record without a string `reason` is dropped. Errors can attach the whole line. Unknown levels go to detail with only a stdout line number. `src/build.rs` copies the field checks, renders every non-warning level as an error, has a private 12-column status parser, and filters failure stderr by first word. Successful generic builds and probes skip the warning replay. Check failure replay reparses lines using line 1 as the synthetic input. `OwnerIndex` collapses `..` lexically and can classify a path as external from its spelling. Member-check reads English Cargo failure text for package-mode support. Completion-record defects are in `bugs.md`.
Enforcement: one streaming event decoder with unknown, malformed and complete states, typed projections per consumer, and cross-consumer transcript fixtures.
Reported by: check-orchestration, convention-gates, cargo-integration, launch-and-install, host-fs-output.

## CARGO-012 - Target and build-dir isolation pins only one directory on many paths

Sites: `src/build.rs`, `src/runnables.rs`, `src/build_purpose.rs`, `src/build_purpose/environment.rs`, `src/check_cmd/fixture_lint/mod.rs`, `src/check_cmd/concurrency.rs`, `src/crit_cmd/mod.rs`.
Plain `run --commit` and bench rustflags recipes pin both `CARGO_TARGET_DIR` and `CARGO_BUILD_BUILD_DIR`. Ordinary `build --commit`, `run --with`, serve, bench worktree builds, isolated sweeps, fixture lint and crit pin only the target dir, so a configured build dir stays shared. Metadata and no-build lookup have another target-only override, and runnable discovery passes none. Check's concurrency detector can serialise its own chains on an env or config setting. See also `hygiene-host.md` (disk gate).
Enforcement: one artifact and intermediate directory plan per invocation, consumed by gate, grouping, child env, discovery and cleanup. Tests with independently redirected target and build dirs.
Reported by: convention-gates, cargo-integration, launch-and-install, worktree-clean-git-deps.

## CARGO-013 - The check scheduler's target dir can differ from Cargo's

Sites: `src/check_cmd/concurrency.rs`, `src/check_cmd/phase.rs`, `src/build_purpose/environment.rs`, `src/check_cmd/selection.rs`.
The scheduling key comes from metadata obtained without the lane env. A lane setting `CARGO_HOME` changes the config chain and can change `build.target-dir`. A forwarded serial `--target-dir` moves the build while the support build and `BROKKR_TEST_BIN_DIR` keep the original. Grouping canonicalises existing paths and falls back to the spelling on failure, so aliases can land in separate chains, and each key gets its own thread with no pool. Build scripts writing outside `OUT_DIR` are delegated to `serial_builds`.
Enforcement: resolve root and build dir once with admitted CLI overrides, use a bounded worker pool, and treat unknown write sets conservatively.
Reported by: check-orchestration.

## CARGO-014 - Plan, build, listing and execution observe configuration separately

Sites: `src/check_cmd/prepare.rs`, `src/check_cmd/test_phase.rs`, `src/check_cmd/diagnostics.rs`, `src/check_cmd/selection.rs`, `src/test_cmd/plan.rs`, `src/test_cmd/run.rs`.
Preparation resolves `lane_env` to build, then again when listing. Test-lint announcements compute a sink from ambient config while placement uses the sweep env. Focused `test` runs support builds before refusing `--timeout` on a doc-only sweep, and builds direct-lane binaries before validating runner, target and launch config. Later sweeps can refuse after earlier ones built. It arms a phase-only watchdog before selection and metadata work, which then runs without an active phase clock.
Enforcement: resolve and validate facts for every attempted lane before any phase starts, under a planning deadline, and make changes an explicit invalidation.
Reported by: check-orchestration, test-runner.

## CARGO-015 - Rustflags cache keys share a namespace with two hash implementations

Sites: `src/build.rs`, `src/config_parts/check.rs`, `src/target_namespace.rs`, `src/clean.rs`.
Sweep and bench keys each implement FNV-1a. Bench keys length-prefix tokens and are domain-separated. Sweep keys join tokens with a space, so `["--cfg", "a b"]` and `["--cfg a", "b"]` share a directory (the code documents this). Bench keys hash requested additions, not the resolved compiler or inherited flags. Cleanup claims the common prefix in the current target root.
Enforcement: one versioned, framed, domain-separated key codec with golden vectors. This costs a cold build, with no TOML change.
Reported by: launch-and-install, config-cli-startup.

## CARGO-016 - Reserved target-directory names are weaker than ownership

Sites: `src/target_namespace.rs`, `src/clean/layout.rs`, `src/clean.rs`, `src/check_cmd/fixture_lint/mod.rs`, `src/test_scratch.rs`.
The reservation applies to profiles selected through brokkr. Cargo or another user of a shared target root can create `fixture-*`, `unify-*` or `rustflags-*` dirs that cleanup will claim. Comparison is case-sensitive, so two fixture names that differ only in case alias on a case-insensitive store. The bare prefix is rejected as a profile but skipped by cleanup. `read_dir` errors are lost and size walks undercount silently. Removed fixture entries leave output until an explicit clean. `target/test-tmp` is spelled by both the scratch allocator and the fixture classifier. The guard probe script writes a checkout-keyed dir under the target root under a name absent from the namespace.
Enforcement: allocate isolated outputs in a brokkr-owned cache root and clean only through it, or through positive ownership records.
Reported by: convention-gates, cargo-integration, test-runner, worktree-clean-git-deps, launch-and-install, lock.

## CARGO-017 - Cargo config stripping has gaps at its boundary

Sites: `src/cargo_config_strip/transaction.rs`, `src/cargo_config_strip/mod.rs`, `src/rustflags.rs`, `src/git/tree_state.rs`.
Recovery and Git cleanliness share reserved suffixes but build the names separately. A symlinked config file is rejected, but a symlinked `.cargo` directory isn't. The legacy selector opens before checking type. Installation needs Linux `renameat2(RENAME_NOREPLACE)` and has no preflight for it. The filtered inode copies mode bits only. `regular` checks metadata and reads the path separately. Recovery deletes a file at the scratch name based on the reservation, which doesn't prove who created it. Directory-open failure during sync is silent, while sync failure warns. The success count comes from the first scan while install runs a second. The source-change warning omits the tree and reports `undecided` as a source. Recovery keeps only the first failure across the two names.
Enforcement: prefer a brokkr-owned checkout or private Cargo config. Otherwise one descriptor-relative transaction with identity checks and fault tests.
Reported by: convention-gates, cargo-integration, config-cli-startup.

## CARGO-018 - Tree policy edits the user's checkout under a lifetime set by convention

Sites: `src/tree_policy.rs`, `src/context.rs`, `src/lock/hold_protocol.rs`, `src/lock/mod.rs`, `src/main_parts/dispatch.rs`, `src/toolchain.rs`.
`HeldTree::activate` takes a path and policy with no lock capability. Worktree code registers the path separately and relies on declaration and drop order. In primary acquisition, edits happen before the capability is published. Watchdog restoration does its own active-hold lookup with a blocking mutex before its five-second wait starts, restores the toolchain first (the opposite of the normal guard), and suppresses thread-start failure and timeout. Editors, Git and other tools that don't take the hold can see or race the edits. A read-only checkout refuses at activation. Toolchain activation messages name only the basename. Pin restoration defects are in `bugs.md`.
Enforcement: prefer explicit toolchain selection and generated Cargo overrides under project state. For unavoidable edits, one transaction owner holding registration, restore and no-clobber authority, with fault tests.
Reported by: convention-gates, cargo-integration, lock, config-cli-startup, host-fs-output.

## CARGO-019 - [cargo_env] admission is process-global and timing-dependent

Sites: `src/project.rs`, `src/cargo_env.rs`, `docs/brokkr.toml.md`.
Project detection installs a global registry, and the spawn path owns a global phase clock and a sticky first failure. Concurrent spawns can both pass the failure check before either fails, and a successful one can spawn after the other sealed the run. The failure path prints while holding `FAILED`, and config-walk errors escape through `?` before becoming sticky. That differs from the documented run-wide refusal. A misspelled `if_exists` unsets the variable and carries on. Static definitions and conflicts are first validated on the build that uses them. See also `bugs.md` (help scan, evaluation order) and `hygiene-process.md` (value exposure).
Enforcement: a command-owned context with explicit admission at spawn, typed preparation failures, and event emission outside the lock. Validate static definitions at planning.
Reported by: cargo-integration, launch-and-install, config-cli-startup, check-orchestration.

## CARGO-020 - Plain run evaluates build inputs twice and checks its runner late

Sites: `src/runnables.rs`, `src/build.rs`, `src/process/spawn.rs`, `docs/brokkr.toml.md`.
The captured build and the quiet run each evaluate `[cargo_env]`, so a changing generator can force a rebuild that's counted in the runtime and launch a different artifact. Probed build and probe evaluate once. Plain run validates its runner after the captured build against the caller's env pairs, not the computed ones. A blocked `--commit` or failed host probe surfaces only after compilation.
Enforcement: freeze each leg's inputs before preflight, and require a successful preflight value at spawn.
Reported by: launch-and-install.

## CARGO-021 - Profile precedence has two implementations

Sites: `src/runnables.rs`, `src/build_purpose.rs`, `src/passthrough_cmd.rs`.
Plain run and install use `build_purpose::profile`. Build, `run --with`, serve and bench use `runnables::resolve_profile`, then build admits that answer again. A conflicting `profile` and `debug` can warn twice in one paired run. Passthrough hardcodes release and picks up feature defaults and package names separately.
Enforcement: one resolved profile per program recipe, with a table-driven test per entry point.
Reported by: launch-and-install.

## CARGO-022 - Artifact selection reinterprets Cargo after it builds

Sites: `src/executable_artifact.rs`, `src/build.rs`, `src/check_cmd/binaries.rs`.
The parser has its own partial-version, URL, legacy-id and glob grammar. `BuildConfig` permits both bin and example, which puts both selectors on argv while selection prefers the bin. An executable record has no root-unit provenance, so an artifact dependency's executable can be returned. Passthrough `--no-build` constructs a host-layout path and can run stale bytes. The legacy space-separated package-id branch has no evidence of current use, and no evidence that it's dead.
Enforcement: resolve to a typed package and target before spawn, make bin and example exclusive, and have no-build consume a published record.
Reported by: launch-and-install, check-orchestration.

## CARGO-023 - Probe freshness and executable identity are checked at different times

Sites: `src/launch/probed.rs`, `src/main_parts/bootstrap.rs`.
Every `compiler-artifact` must be `fresh: true`, but no digest is bound at handoff. The parent stats the probed and built paths, then opens them separately. Failure messages don't name the pair. `--with` builds both sides before probing, and recipes sharing an uplift path can refuse after both builds. Focused `test` similarly opens executables by path after verification and reuses paths across repeats.
Enforcement: capture identity at handoff, compare opened files, and isolate same-output recipes.
Reported by: launch-and-install, test-runner.

## CARGO-024 - The launch helper protocol is spelled on both sides

Sites: `build.rs`, `src/launch/mod.rs`, `src/launch/helper_main.rs`, `src/launch/probed.rs`, `src/maintenance.rs`.
Parent, build script, helper and cleaner each spell part of `brokkr-launch`, `--plan`, `--probe`, the peer basename and `.brokkr-plan` suffix, receipt suffix and contents, magic and tags, store names, and the plan size formula (12 MiB + 64 KiB, which agree today). The parent rejects a non-absolute peer executable while the helper checks only presence. The helper accepts repeated scalar tags as last-wins but rejects duplicate env entries. The build script lists supported architectures separately from the syscall code. Probe and receipt parse errors lack the record path, tag or position.
Enforcement: a `core`-only protocol source compiled into both units and tracked as a build-script input, plus decoder rejection and round-trip tests against the real helper.
Reported by: cargo-integration, launch-and-install, process-and-reservations, bench, config-cli-startup, host-fs-output, worktree-clean-git-deps.

## CARGO-025 - The launch helper implements host process policy by hand

Sites: `src/launch/helper_main.rs`, `src/launch/helper.rs`, `build.rs`.
It assumes Linux x86_64 or aarch64 ABIs, readable `/proc/self/exe`, bounded paths and cwd (4096 bytes), an executable filesystem under `HOME`, `/bin/sh`, and a glibc-style default `PATH` (`/bin:/usr/bin` when unset, cwd when empty). It has its own PATH search, `ENOEXEC` fallback and errno policy. It calls `getcwd` even when keeping cwd. The fixed arrays reserve about 66 MiB of address space. The platform gate and ELF check live apart, and the ELF test reads little-endian headers. Raw global scratch references rely on single-threaded call order. Helper write failures are ignored. `build.rs` compiles it with Cargo's `RUSTC` (or literal `rustc`) and fixed flags, bypassing wrappers, the target linker and encoded rustflags.
Enforcement: one supported-platform declaration, a host capability check before building, build-time ELF validation, cross-architecture process fixtures, and a declared helper toolchain contract.
Reported by: launch-and-install, cargo-integration, config-cli-startup, host-fs-output.

## CARGO-026 - The launch receipt conflates arrival, exec and exit

Sites: `src/launch/helper_main.rs`, `src/runnables.rs`.
The helper reads `/proc/self/exe`, inspects arguments, builds the receipt path and opens it before writing `started`, and any of those can fail, so a missing receipt doesn't prove the helper never ran. The parent treats any content other than `started\n` as failure and waives a missing receipt for exit 101, which can't tell a Cargo failure from a program that returns 101. A failed `failed` append leaves `started`. Runtime narration prints before classification, and the runtime line can say the target finished when Cargo failed first.
Enforcement: typed handoff states with a separate setup-failure channel, and a transcript test for missing, partial and failed handoffs.
Reported by: launch-and-install.

## CARGO-027 - "Full environment" has three meanings

Sites: `src/launch/helper_main.rs`, `src/launch/probed.rs`, `src/serve.rs`.
Helper inherit mode keeps untouched raw env entries. The probe parser rejects malformed or duplicate entries. Peer merge drops malformed entries and takes the first duplicate. Serve's saved environment shares part of the probe decoder with a different envelope.
Enforcement: one documented OS environment model with shared validation, and the ingress differences expressed as policy.
Reported by: launch-and-install.

## CARGO-028 - Jobserver and ownership variables share string-level handling

Sites: `src/launch/probed.rs`, `src/serve.rs`, `src/process/spawn.rs`, `src/main_parts/bootstrap.rs`.
Direct launch discards probe jobserver strings and applies live values last. A peer takes live jobserver keys and pins ownership keys. Serve serialises the outer map for a detached supervisor. Nothing checks that the named descriptors or FIFO are still valid at exec. `freeze_env` stamps capability and cohort before the build, so build, probe and program carry the recorded cohort instead of being stamped at their own spawn, and `[cargo_env]` evaluation precedes that stamp. See also `hygiene-process.md` (child environment keys).
Enforcement: separate verified live jobserver handles, detachable environment and hold-bound data in the API.
Reported by: launch-and-install.

## CARGO-029 - Install guesses what Cargo published

Sites: `src/build_info.rs`, `src/runnables.rs`, `src/config_parts/parser.rs`, `docs/commands/run.md`.
`install_root` copies the nonempty `CARGO_INSTALL_ROOT` and `install.root` rules, then forces `--root` or skips stamping. `installable_bins` supplies lower and upper default-feature closures and refuses dependency-feature ambiguity. That ambiguity fails `--names` but only warns after a real install. Duplicate configured package entries are accepted and installed repeatedly. Each package's root is resolved inside the publication loop, `--locked` is decided from the conventional `Cargo.lock`, and `objcopy` and ELF errors appear after Cargo installs. A later failure leaves earlier binaries replaced, with no resume record.
Enforcement: consume the resolved install plan and Cargo's publication ledger, and preflight the whole plan before any publication.
Reported by: launch-and-install.

## CARGO-030 - Install stamping writes into Cargo's install tree

Sites: `src/build_info.rs`, `src/atomic_write.rs`.
Stamping creates PID/name scratch in Cargo's `bin`, runs `objcopy`, checks, chmods and renames, with no explicit sync, no binding to the file Cargo published, and no metadata contract. A killed process leaves scratch in another tool's directory.
Enforcement: a brokkr install staging area and a destination backend with explicit authority over the install root.
Reported by: launch-and-install, host-fs-output.

## CARGO-031 - Install copy steps: admission and transaction boundaries

Sites: `src/install_step/mod.rs`, `src/install_step/tree.rs`, `src/install_step/transaction.rs`, `src/install_step/registry.rs`, `src/config_parts/parser.rs`, `docs/commands/run.md`.
Config rejects absolute, `.` and `..` sources, but an intermediate symlink can take `copy.from` outside the project, and containment isn't rechecked after canonicalisation. Destinations follow symlinked parents and overlap guards compare pathnames. The registry owner is project name plus step name, so two checkouts collide and a rename creates a foreign claim. Recovery trusts stored roles, container names and fingerprints. Staging, fingerprinting and cleanup reopen by name, the no-follow-open check is written twice, and `Allocated` recovery has no recorded identity. `renameat2` and directory fsync are discovered during publication. `recover_all` warns and returns success, `same_dir` discards I/O errors, and some errors lack operation and path. Tree copy recurses with no bound. Settled claims stay after the destination is gone.
Enforcement: typed relative grants, descriptor-relative containment, validated journal records, a structured recovery report, and filesystem fault fixtures.
Reported by: launch-and-install.

## CARGO-032 - Two answers to "which commit, and was it clean?"

Sites: `build.rs`, `src/build_info.rs`, `src/git/tree_state.rs`, `src/git.rs`, `src/version_cmd.rs`, `src/visual/engine.rs`.
`build.rs` runs plain porcelain with a nine-character minimum. Install takes a full commit with `TreeState::for_install`, forcing submodule reporting and excluding brokkr's own mutations. `--version` shortens to eight characters while the git module defines nine, and visual approval stamps nine. `build.rs` rerun hints cover a loose HEAD ref, index, `packed-refs` and selected paths, and assume Git on PATH. The build-script status defect is in `bugs.md`.
Enforcement: one provenance vocabulary with observation time, dirt policy and an `Unknown` cause, sharing the codec and width across the build-script boundary.
Reported by: launch-and-install, visual-projects.

## CARGO-033 - The build stamp format has several owners

Sites: `src/build_info.rs`, `src/elf_section.rs`, `src/bin/results_server/identity.rs`, `src/version_cmd.rs`.
Install and the results server both spell `.brokkr.build_info`, though both already use the shared ELF reader. The reader accepts any single line starting `sha `. The installer writes full hex plus `clean y|n`, and the formatter treats anything lacking ` clean y` as dirty, so malformed provenance shows as a real commit or dirt. The ELF reader needs 64-bit little-endian ELF with section headers and allocates the whole section table and a header-sized name table before range checks. Matching sections accumulate. Its self-reader needs `/proc/self/exe`. The server reimplements the xxh3 streaming loop.
Enforcement: section id, `BuildStamp` codec and a bounded reader in the shared module, with golden ELF and malformed-stamp fixtures.
Reported by: launch-and-install, bench, results-server-and-stores, host-fs-output.

## CARGO-034 - Companion binary discovery has several owners

Sites: `src/version_cmd.rs`, `src/guard.rs`, `src/fstrim.rs`, `src/cli/schema.rs`, Cargo targets.
Version repeats the guard and server names and looks beside `current_exe`. Guard enrolment builds the sibling path itself, which can differ from the guard Cargo actually uses or the server actually deployed. The server name also appears in the Cargo target, clap metadata and two renderers. The fstrim install hint repeats `/usr/local/libexec/brokkr-fstrim` and assumes `~/.cargo/bin`.
Enforcement: one packaging manifest for companion names and paths, with separate shipped and active identities.
Reported by: launch-and-install, config-cli-startup, results-server-and-stores, process-and-reservations.

## CARGO-035 - Passthrough is a live parallel launch implementation

Sites: `src/passthrough_cmd.rs`, `src/cli/schema.rs`, `src/main_parts/dispatch.rs`.
It has a CLI entry and dispatch caller, so it's live. It uses a release build or an inferred existing binary, runs it bare, has its own timing and output (raw milliseconds), resolves features for the host, and creates scratch eagerly. Its executable path goes through `display()`, and publication and clearing are separate through an optional guard.
Enforcement: keep its capabilities through the normal entry points in integration tests, then a module dependency rule keeping launch in one place.
Reported by: launch-and-install.

## CARGO-036 - Historical builds depend on today's host and checkout

Sites: `src/worktree.rs`, `docs/commands/run.md`, `docs/commands/serve.md`.
Worktree furnishing links live sibling checkouts and supports one-level-up dependency paths, reusing existing links. Untracked conventional lockfiles can be seeded from the live tree. Runner cwd moves to the live tree while recorded loader and manifest paths still reach the build tree. A serve keeps the executable while later builds or eviction can change a library it loads.
Enforcement: declare historical inputs, resolve actual roots before admission, and use leases or immutable runtime bundles.
Reported by: launch-and-install, worktree-clean-git-deps.

## CARGO-037 - Removable shapes in the build and launch code

Sites: `src/build.rs` (`project_info`), `src/runnables.rs`, `src/launch/mod.rs`, `src/run_invocation.rs`, `src/db/write.rs`, `src/cargo/metadata.rs`, `src/cargo_filter.rs`, `src/check_cmd/test_phase.rs`, `src/check_cmd/parallel.rs`, `src/check_cmd/selection.rs`, `src/check_cmd/prepare.rs`.
Every caller of `project_info` passes `Some(root)`. Production plain-run plans leave `sets` empty, so the inherit-mode overlay is unused in production. `run_invocation::capture` computes a raw arm key that the writer uses only as a presence check, and bootstrap builds a `RunInvocation` for every command and can reject one on old arm-key assumptions (see `hygiene-stores.md`). `CargoTarget::harness` has no reader. The optional `OwnerIndex` is `Some` only in tests. Several `dead_code` attributes are stale the other way. Dispatch has a fallback to budget one for an impossible non-parallel `LaneKind::Parallel`. The optional resolution request in the parallel repro allows a reduced `-p` reconstruction. Preparation hands a build to `lane()` through a mutable `pending` slot checked by label at runtime.
Enforcement: require an explicit root, typed plan modes, and owned handoff types (`ResolvedLane` to `BuiltLane` to `ListedLane`), and drop stale suppressions.
Reported by: launch-and-install, cargo-integration, check-orchestration.

## CARGO-038 - Forwarded arguments and listings are reinterpreted

Sites: `src/check_cmd/selection.rs`, `src/check_cmd/binaries.rs`, `src/check_cmd/prepare.rs`, `src/check_cmd/isolate.rs`, `src/test_runner/invocation.rs`, `src/test_cmd/focused.rs`, `src/test_cmd/plan.rs`.
`ForwardedArgs` publishes a classification, but the binary selector path parses target flags again. Isolated execution strips positionals with its own option-arity list. The ignored-only listing strips flag-looking tokens without knowing option values. Thread-count argv scanning is duplicated, and `u32` allows zero where nextest treats zero as default. `--timeout` is range-checked separately in clap and the handler. Discovery and execution spell the nightly JSON options separately. The parallel repro drops forwarded libtest flags and the effective env.
Enforcement: typed options, one listing result with checked counts, and table and property tests.
Reported by: check-orchestration, test-runner.

## CARGO-039 - Phase and shape identity have parallel definitions

Sites: `src/check_cmd/check_plan.rs`, `src/check_cmd/phase.rs`, `src/check_cmd/selection.rs`, `src/test_runner/plan.rs`.
The same skip predicate is written in planning, execution and the plan accessor, and audit has its own disposition match. Execution-only doctest checks run after planning, so audit can show a lane the run would refuse. `SelectionPhase` spells three phase names. Declared and effective build identities each enumerate shared fields with their own encoding. Hash-site cardinality lives in both the enum and a nine-slot stats array.
Enforcement: one typed phase plan with disposition and cause, and enum-indexed storage.
Reported by: check-orchestration, test-runner.

## CARGO-040 - Worktree link and container used as ownership records

Sites: `src/worktree.rs`.
Furnishing trusts an existing `.brokkr-target` and creates its destination. Discovery infers isolation from lexical position under the container. An old slot ignores a change in Cargo placement. The removal defect is in `bugs.md`.
Enforcement: a slot record with full identity and a verified allocation under a brokkr-owned cache root, with the link derived from it.
Reported by: worktree-clean-git-deps, cargo-integration.

## CARGO-041 - The worktree sibling-dependency resolver has its own manifest rules

Sites: `src/worktree.rs`, `src/deps/workspace_dep.rs`.
It uses line-oriented `git ls-files` and lossy text, so quoted or non-UTF-8 paths drop out, and read or parse failures vanish. It treats tables named `dependencies` under arbitrary metadata as real dependencies, unlike the Cargo-aware walker. It trusts a same-named live sibling, skips an existing slot entry without checking its destination, and leaves absolute dependencies in place.
Enforcement: NUL-framed native paths, one manifest model, and a recorded placement plan.
Reported by: worktree-clean-git-deps.

## CARGO-042 - Worktree lockfile provenance is inferred apart from Cargo

Sites: `src/worktree_lock.rs`, `src/git/tree_state.rs`, `docs/commands/run.md`.
Seeding scans manifests and ancestors itself. Unreadable ancestors become "no workspace", `resolver.lockfile-path` is invisible, and placement assumes no workspace manifest above the container. `present` refuses symlinks while `digest` follows them. The seed lands as an untracked `Cargo.lock`, which reuse accepts and retention may count as dirt.
Enforcement: one Cargo-derived lock location and a typed provenance record for admission, hashing and deletion.
Reported by: worktree-clean-git-deps.

## CARGO-043 - Worktree retention and removal depend on call order

Sites: `src/context.rs`, `src/worktree_record.rs`, `src/worktree.rs`, `src/reservations/mod.rs`, `docs/brokkr.toml.md`.
`create`, `purge_all`, `enforce` and `Store::touch` take no hold capability. The read-modify-write of `worktrees.toml` depends on the wrapper. Eviction ignores active-tree information when its lookup returns `None`. Removal deletes the target first, then calls Git under both reservation serial locks, and every Git error leads on to filesystem deletion, while the `git worktree prune` result is discarded. Retention runs only on a cut, per checkout, can skip dirty or unreadable slots forever, and has no byte budget. LRU uses wall seconds. Purging leaves bookkeeping behind.
Enforcement: a worktree manager with typed hold and use leases, a journalled removal, and bounded subprocesses outside the reservation lock.
Reported by: worktree-clean-git-deps, process-and-reservations, config-cli-startup.

## CARGO-044 - Git has one supervisor and several command contracts

Sites: `src/git.rs`, `src/git/tree_state.rs`, `src/worktree.rs`, `src/worktree_lfs.rs`, `src/env.rs`, `src/scope.rs`, `src/gremlins.rs`.
Commit-subject and HEAD lookups are duplicated. Worktree and LFS readers repeat spawn, status and stderr handling and often report just `git`. The status reader collapses spawn failure, interruption, nonzero status and bad output into `None`, which admission calls "modified" and retention calls "uncommitted work". The inventory refuses the whole walk on any non-UTF-8 name, while status carries it and drops diagnostic priority. From a subdirectory, porcelain is checkout-relative and `ls-files` is invocation-relative. Unmerged stages can repeat an inventory path.
Enforcement: Git operations around repository identity with native argv, a deadline and typed outcomes. Tests for subdirectory, conflicted-index, non-UTF-8 and missing-Git cases.
Reported by: worktree-clean-git-deps, convention-gates, host-fs-output.

## CARGO-045 - Tree-state policies differ between consumers

Sites: `src/globs.rs`, `src/git/tree_state.rs`.
Check recognises `.md` and `.markdown` case-insensitively, while measurement hardcodes lowercase `.md`. Install overrides the submodule ignore policy, while measurement, reuse and deletion inherit it. Measurement exempts all of `.brokkr`, tracked files included, while install exempts only untracked state. Reuse accepts three generated-file status shapes without provenance. `ls-tree` / `show` / executable-bit verification is duplicated. The approved-image exemption matches `snapshots/*/approved.png` in every project and compares checkout-relative names against a config-rooted layout.
Enforcement: central evidence for owned mutations, with each consumer declaring its exemptions, and a policy matrix test.
Reported by: worktree-clean-git-deps, visual-projects.

## CARGO-046 - LFS admission speaks with more certainty than it has

Sites: `src/worktree_lfs.rs`.
It inspects blobs up to 1,024 bytes regardless of attributes, so a missing large tracked file never reaches the missing check. It owns three version URLs and its own grammar. Unknown keys reject silently and repeated fields overwrite. The warning says "unreachable" for what could equally be a skipped smudge or committed pointer text, and prints up to ten raw paths on one line with no checkout.
Enforcement: separate pointer state from hydration provenance, use LFS contract fixtures, and a bounded escaped renderer.
Reported by: worktree-clean-git-deps.

## CARGO-047 - Cleanup's ownership claims exceed its evidence

Sites: `reference/clean.md`, `src/clean/layout.rs`, `src/clean.rs`, `docs/commands/clean.md`.
Prefix matches claim any directory with a nonempty suffix. The retired library sweep accepts anything two levels under `scratch/library`. Legacy sibling slots are inferred from names. Pbfhogg's configured-input exclusion is lexical and covers only PBF and OSC, so an alias or case-folding filesystem defeats it. Pbfhogg scratch and worktrees go through the reservation gate, but rustflags, fixture and unification targets, hung snapshots, build snapshots and litehtml state are removed directly, though the docs describe run-artefact trees as gated. A real `--cargo` sweep runs first, and routine cleanup requires metadata, so a broken manifest blocks unrelated reclamation. The prose says clean runs in any project, but dispatch requires detection. Purge prints "removing" before knowing whether a lease prevents it, and several sweeps report a count without the root.
Enforcement: a dedicated brokkr cache root, a typed cleanup plan with a read-only planning pass, module-level mutation access, and independent step results.
Reported by: worktree-clean-git-deps, cargo-integration, process-and-reservations, visual-projects.

## CARGO-048 - Maintenance repeats writer literals and hides scan failures

Sites: `src/maintenance.rs`, `src/launch/mod.rs`, `src/launch/probed.rs`, `reference/clean.md`.
It repeats `plans`, `probes`, `.brokkr-launch` and `.brokkr-peer`, treats unreadable directories as empty, and can descend through a symlinked snapshot entry before checking type. Recursive byte accounting and deletion run under the global lock before the command. Old helper digests and failed helper staging aren't swept, and foreground peer leftovers are swept only under the selected project root.
Enforcement: each writer owns its namespace and provides a recovery enumerator with absent, partial and inaccessible outcomes.
Reported by: worktree-clean-git-deps, launch-and-install.

## CARGO-049 - fmt and wc leave runtime choices to callers

Sites: `src/main_parts/bootstrap.rs`, `src/runnables.rs`, `src/wc.rs`, `src/gremlins.rs`.
Ordinary `fmt` can rewrite source while a locked build is reading it. `wc` skips every read failure, reads each file fully as UTF-8, can then report "no rust files", and one non-UTF-8 Git path refuses the whole inventory. The lock-policy defect is in `bugs.md`.
Enforcement: a typed formatting request that tells a check from a writer and requires a source-write lease, and a native-path streaming counter.
Reported by: worktree-clean-git-deps.

## CARGO-050 - Deps graph projection has its own view of the host

Sites: `src/deps/mod.rs`, `src/cargo/metadata.rs`, `src/deps/duplicate_version.rs`, `src/deps/focus.rs`.
It projects with the triple from `rustc` on PATH, retains unknown `cfg` expressions without saying the projection is partial, and uses the in-process view for native code too. Focus and duplicate blame inherit the lossy BFS. Focus headers and chains have no width limit and can omit source identity. Defects are in `bugs.md`.
Enforcement: target selection as an explicit audit input, projection uncertainty in the model, and differential fixtures against Cargo.
Reported by: worktree-clean-git-deps, cargo-integration.

## CARGO-051 - Deps source classification is split among renderers

Sites: `src/cargo/metadata.rs`, `src/deps/git_dependency.rs`, `src/deps/focus.rs`, `src/deps/render.rs`, `src/deps/phase.rs`, `src/deps/workspace_dep.rs`, `src/deps/mod.rs`, `reference/deps.md`, `scripts/publish-cycle-fixture.py`.
Metadata guesses unfamiliar sources are registries. Full deps hand-splits Git URLs with its own tag, branch and rev precedence and accepts empty values. Focus prints raw Git sources and recognises the two crates.io spellings separately. Path reporting is absolute in full deps and tilde-substituted in focus. `workspace_dep_ignore` is stored raw and validated after metadata, or not at all in focus mode. Metadata and deps each own the `--locked` hint. `OutdatedComplete` and the flat `rev` / `branch` / `tag` fields are compatibility-only. Phase order is positional. The publication fixture has no configured script check.
Enforcement: one parsed source with an unknown case, structured paths, an NDJSON schema fixture, and phase metadata from one catalogue.
Reported by: worktree-clean-git-deps.

## CARGO-052 - Crit baselines can describe a different tree from the one measured

Sites: `src/crit_cmd/mod.rs`, `src/crit_cmd/discover.rs`, `src/crit_cmd/stamp.rs`, `src/worktree.rs`, `reference/crit.md`.
`crit` resolves the ref, or live HEAD and dirt, before locking, and the worktree path resolves it again. A ref moving in between labels one commit and measures another. Criterion writes each benchmark separately while brokkr replaces one global stamp per label, so a stamp can vouch for mixed data. Empty-run detection scans the whole store twice and treats a changed mtime as the only evidence of a rewrite. `stamps` and `.txt` are spelled in two places. The stamp captures ambient `RUSTFLAGS` and the rustc on PATH after the run, resolves a relative `CARGO_HOME` against brokkr's cwd, pairs ancestor configs by depth, and parses an empty or malformed stamp as valid. Crit echoes forwarded arguments and prints raw flags. Duplicate target names are pushed back to the operator, though the owning package is known. `artefacts::validate_test_id` is referenced with no implementation.
Enforcement: resolve a full commit once, a package/target-keyed store of immutable completed measurements, and versioned typed provenance captured from the effective launch.
Reported by: worktree-clean-git-deps, cargo-integration.

## CARGO-053 - Shell gates and computed env inherit host behaviour

Sites: `src/script_check.rs`, `src/output/child.rs`, `src/cargo_env.rs`.
`sh` comes from PATH, with the code tree as cwd and the ordinary environment. A script's `cargo` inherits the hold capability but enters as a plain child, bypassing brokkr's Cargo env evaluation, so the same Cargo command can see different inputs in a script gate and in native rustdoc or fixture lint. `[cargo_env]` commands inherit the host shell and can write anywhere the account can. Relative paths with `..` or symlinks can consult locations outside the tree.
Enforcement: an explicit execution context, structured argv or Cargo gate kinds, and fake-tool tests for cwd and env precedence. Arbitrary shell effects need an OS boundary.
Reported by: convention-gates, cargo-integration.

## CARGO-054 - Rustdoc capability probe and launch resolution disagree

Sites: `src/rustdoc_check.rs`, `src/rustflags.rs`, `src/check_cmd/diagnostics.rs`.
The probe takes the first explicit `RUSTDOC`, then process `RUSTDOC` (UTF-8 only), then `rustdoc` on its PATH, and tries an empty value as a program. Launch config uses the last explicit value, keeps native strings, and also reads `CARGO_BUILD_RUSTDOC` and `build.rustdoc`. The probe runs without the sweep's PATH, toolchain or Cargo env, and a process-global `OnceLock<bool>` gives every sweep the first answer, which concurrent groups race for. Spawn failure, odd help text and nonzero exit all select rendering. The probe has no deadline, and its flags are spelled separately from execution's.
Enforcement: probe the resolved executable with the resolved env, cache keyed by tool identity, and return supported, unsupported or failed with a reason.
Reported by: check-orchestration, convention-gates.

## CARGO-055 - Rustdoc freshness stamps write into Cargo's doc tree

Sites: `src/check_cmd/diagnostics.rs`, `src/rustdoc_check.rs`, `src/target_namespace.rs`.
The gate reconstructs `target[/triple]/doc/<crate>/index.html`, creates a placeholder or rewinds the mtime of an existing page, before checking whether Cargo succeeded or whether the unit got `--check`. The location comes from the sweep's explicit target only, so an ambient target, custom naming or proc-macro units misplace it. The existence test and write are separate, paths follow symlinks, and every file-operation failure disappears.
Enforcement: a brokkr-owned target namespace for lint-only rustdoc, or freshness evidence in project state with Cargo owning its docs.
Reported by: check-orchestration, convention-gates.

## CARGO-056 - Fixture lint state and isolation

Sites: `src/check_cmd/fixture_lint/mod.rs`, `src/check_cmd/fixture_lint/walk.rs`, `src/config_parts/parser.rs`, `src/check_cmd/phase.rs`, `src/atomic_write.rs`, `docs/brokkr.toml.conventions.md`.
The expectation manifest is read after seeding and lock resolution, so a typo surfaces late and after changing the fixture checkout. Preflight checks that `[workspace]` keys exist, not that the table is empty. A default `when = "gate"` fixture can be stranded without a complete profile. Preflight never checks the lock target, and the atomic writer follows an existing symlink, so the seeded `Cargo.lock` can escape the fixture. Variants can change PATH or `CARGO_HOME` while fixture shape is resolved once under the base env, and the shared Cargo hook can override supplied values before `unset` applies. The disk walk drops `read_dir` errors and follows directory symlinks with no visited set. The module walk can reach files outside via `#[path]`, and the final comparison checks only one direction. The lib kinds are enumerated here and in member checks. The project also has two external scripts checking `clippy.toml` equality and nested-config shadowing.
Enforcement: validate static fixture inputs before expensive work, a brokkr-owned fixture workspace under project state, one typed invocation per variant, and a bounded rooted inventory compared in both directions.
Reported by: convention-gates, cargo-integration, host-fs-output.
