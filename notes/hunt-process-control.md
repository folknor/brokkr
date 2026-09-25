I found 50 findings across the eight questions. Twelve of them are live bugs or doc claims that are false today; I list those first, then everything by question. I only read code and ran nothing; findings that depend on an ordering or a race were traced through the code, not reproduced.

**What this build can already enforce.** The repo's `brokkr.toml` has only three `[[textlint]]` rules (no line numbers in docs; no `notes/` citations in docs or comments). There is no `[[script_check]]`, no `[[dependency_rule]]`, no `[header]` and no `clippy.toml`, so clippy's `disallowed_methods` is unused. `Cargo.toml` `[lints.clippy]` is a deny-list of generic lints. The one domain-specific mechanism is `RESERVED_SWEEP_ENV` in `src/config_parts/parser.rs`, which refuses `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS` and `CARGO_TARGET_DIR` in `[[check]] env`. The engines are strong and the rule set is thin: almost everything below could be held by a textlint rule, a `disallowed_methods` entry, a shared module, or one test.

## Live bugs and false claims

1. **Measurement paths skip the stray reap and the stale-guard warning.** Both only run inside `context::acquire_cmd_lock_opt`. At least 12 sites call `lockfile::acquire` directly and get neither:
   - `BenchContext::with_build_config`, `HarnessContext::new`, `BenchHarness::new`
   - `pbfhogg/verify.rs`, `piners/cmd.rs`, `piners/lint/cmd.rs`
   - ratatoskr `service_suite`, `service_test`, `saehrimnir`, `list_smoke` (twice), `bench_gate`

   CLAUDE.md and `docs/commands/check.md` §Strays both say every locked command reaps. This is false for exactly the commands the reap exists to protect.
   - **Fix:** move the reap and the warning into `lockfile::acquire`'s fresh-hold branch, and make `lockfile::acquire` private to that path. A textlint rule forbidding `lockfile::acquire(` outside one file would also hold it.

2. **Worktree eviction and creation run before the lock.** `context::with_worktree` calls `worktree_record::enforce` and `Worktree::create` (including `git worktree remove --force`) before the closure takes the lock.
   - A second brokkr can evict or recreate a worktree that the lock holder is building in. `is_dirty` cannot see that use: `target/` is gitignored, so an in-use worktree looks clean.
   - The `worktrees.toml` read-modify-write is also unlocked, so updates can be lost.
   - `clean --worktrees`, which is locked, races with it too.
   - **Fix:** take the lock before any worktree work. The toolchain re-arm can be done by re-activating inside the hold instead of by ordering.

3. **Eviction runs on reuse, and the bound is off by one.** `enforce` is called unconditionally, before `create` decides whether to reuse. Both `docs/brokkr.toml.md` §worktree_keep and `docs/commands/clean.md` say "Eviction runs when a new worktree is cut, never on reuse". That is false.
   - The steady state is `keep + 1`: evicting down to `keep` happens before a new one is added.
   - `enforce` can evict the very worktree about to be reused, which then gets rebuilt cold.
   - The `enforce` doc comment says it is "Called from `Worktree::create`"; it is called from `with_worktree`.

4. **`remove_one` is not the shared removal.** CLAUDE.md and the `remove_one` doc say `purge_all` and eviction share it. `purge_all` inlines its own copy of the git-then-filesystem sequence, and `Worktree::create`'s stale branch is a third copy.
   - The copies have already diverged on the rule that matters: eviction never removes a dirty worktree ("correctness, not courtesy"), but `create` force-removes a stale worktree with no dirty check.
   - If `rev-parse HEAD` fails transiently, `create` treats the worktree as stale and force-removes it.

5. **`size_bytes` is never written.** `docs/brokkr.toml.md` says "brokkr caches each worktree's measured size", and the module header says the same. Nothing assigns the field; it is only parsed and re-serialised. It is dead, and the docs claim it is live.

6. **The worktree store is keyed on the wrong root.** `.brokkr/worktrees.toml` lives at `project_root`, but `enforce` lists worktrees per `git_root` and calls `prune_missing` against that one list.
   - Under the config-one-level-up layout where one config directory governs several checkouts (which `build.rs::require_cargo_tree` says happens), each run deletes the other checkouts' records.
   - Their worktrees then sort oldest ("no record") and are evicted first.

7. **Three contradictory rules for which cargo config file wins.**
   - `guard.rs::cargo_config_path`: extensionless `config` wins when both exist. This matches cargo.
   - `bench_cmd/stamp.rs::cargo_config_digest` and `repo_config_digests`: `config.toml` first.
   - `rustflags.rs::push_config` says "cargo … prefers `config.toml`". That is wrong, and it is out of my scope but shares the fact.

   The stamp therefore digests the file cargo does not read when both exist. `$CARGO_HOME`→`$HOME/.cargo` resolution is written three times.
   - **Fix:** one `cargo_config_file(dir)` function, plus a test covering the both-exist case.

8. **`bench` and the harness disagree about what "dirty" means.**
   - `bench_cmd::is_dirty` is raw `git status --porcelain`: untracked files and `.brokkr/` count.
   - `git::check_clean` excludes `.brokkr/`, `*.md`, `brokkr.toml`, `approved.png` and the toolchain sidecar. It was written after the bug where writes to a tracked `gate.db` blocked the next run.
   - `bench` never received that fix. In a repo that tracks part of `.brokkr/` (ratatoskr's `gate.db`) or has an untracked note, `brokkr bench` refuses where the harness would not. Its own baselines under `.brokkr/bench/` dirty the tree unless `.brokkr` is gitignored.
   - There are three more definitions: `worktree_record::is_dirty` (porcelain; failure counts as dirty), `scope::dirt`, and the crate `build.rs` (porcelain, so this repo's build is `-dirty` today because of the untracked notes files).
   - **Fix:** one definition, parameterised by question ("honest commit" vs "would deletion destroy work").

9. **`SigtermGuard` is not re-entrant.** `install` resets `SHUTDOWN_REQUESTED`; `Drop` restores `SIG_DFL` and clears the flag.
   - Outer guards exist in ratatoskr, piners and `list_smoke`. Inner guards exist in `run_passthrough_in` and `run_sidecar`.
   - Any nesting silently drops a pending request, and after the inner guard drops, Ctrl-C kills brokkr outright with no mock teardown or toolchain restore.
   - I did not find a live nesting path today; this is a latent hazard. The lock solved the same problem with refcounting; this guard should too.

10. **Test isolation is claimed and not true.** The `lockfile.rs` tests say "none of them ever touch the real global lock", but `acquire_at` → `drain_and_authorize` → `drain_compile_leases` uses the real `~/.brokkr/compile.lock`.
    - After 20s a unit test calls `stray::reap_for_drain()` and can SIGKILL a real rust-analyzer cargo on the host. That same 20s is the per-test hard cap, so such a test fails anyway.
    - The tests also mutate process-global state without the serialising lock:
      - `hold::publish_capability` / `clear_capability`: `hold::tests::a_second_hold_replaces…` claims it is "Serialized with the other capability-mutating test", but the lockfile tests don't take that lock.
      - `toolchain::DISABLE_DIR`, which `toolchain::tests::arm_drives_activation…` arms while lockfile tests call `activate_for_lock`: a cross-module flake that can rename files in the other test's scratch directory.
    - **Fix:** take the compile-lock path as a parameter alongside the lock path, and make the capability and toolchain slots per-`LockInner` rather than process-global.

11. **Stale claims in `src/bin/rustc_guard.rs`.**
    - The header says "The `/proc` stat parse mirrors `src/stray.rs`" and lists "an unreadable `/proc`" among its fail-open cases. The `auth_hash` doc says it is duplicated "in the same way the `/proc` parse was duplicated". The guard reads no `/proc` any more; the ancestry walk is gone.
    - Crate `build.rs` says ".git is one level up from this build script". It is a sibling, and the code is correct.
    - `rerun-if-changed=.git/HEAD` does not change when a commit moves a branch ref (the ref file changes, not HEAD). Editing an unstaged file does not rerun the script. So "a `-dirty` build is never mistaken for one off a clean commit" is false.

12. **Test-only globals reachable from production.** `acquire_at` is the "unit-test seam" and still runs the global drain, the stray reap and the capability publication. Production and tests share every global.

## 1. One value, one owner

- **Lock protocol literals across the bin boundary.** `rustc_guard.rs` hand-spells `"BROKKR_HOLD_NONCE"`, `"BROKKR_COMPILE_LEASE"`, `"BROKKR_CARGO"`, `".brokkr"`, `"brokkr.lock"`, `"compile.lock"`, the `auth=` key, `auth_hash`, and the handshake line `protocol=1`. These pair with `hold.rs` constants, `lockfile.rs` paths and `guard::GUARD_PROTOCOL`.
  - The stated reason ("no lib target") does not force the copy. A `#[path = "../lock_protocol.rs"] mod protocol;` included by both bins shares constants, `auth_hash`, the path rule and the key names with no lib target.
  - Better still: `build.rs`'s `BROKKR_LONG_VERSION` applies to both bins, so the guard could print its build hash and brokkr could compare it exactly. That replaces the hand-bumped protocol number.
- **`$HOME/.brokkr` resolution is written four times, and the copies already differ.**
  - `lockfile::lock_path` uses `var("HOME")`, which errors on unset or non-UTF-8.
  - `hold::cargo_config_overrides` uses `var("HOME")`.
  - `rustc_guard::brokkr_dir` and `lock_is_held` (a separate copy inside the same file) use `var_os`.
  - Empty `HOME` yields a relative `.brokkr/brokkr.lock` in brokkr's cwd, and in rustc's cwd for the guard. The two sides then lock different files. Nothing rejects empty.
- **`/proc/<pid>/stat` is parsed four times:** `lockfile::proc_starttime`, `stray::read_proc`, `sidecar.rs`, `check_cmd/watchdog.rs::proc_ppid`. Process-tree walks: `stray::collect_descendants` and `watchdog::descendants`.
- **The `cargo metadata --format-version 1 --no-deps` spawn-and-parse block is copied five times:** `build::project_info`, `build::resolve_existing_binary`, `bench_cmd/discover.rs`, `runnables::discover`, `deps`. Each has its own error text. `pbfhogg/bench_all.rs` also hand-rolls `cargo build --release --message-format=json` + `find_executable` instead of `cargo_build`.
- **`rustc -vV` is parsed twice:** `stamp::rustc_version` (in `build_root`) and `rustflags::host_triple` (in cwd, cached per process, so it can report a different toolchain's host).
- **Six "run git, trim stdout" helpers:** `worktree::run_git`, `worktree_record::is_dirty`, `bench_cmd::git` (via `output`), `git.rs` (three), `scope.rs`, `wc.rs`, `gremlins.rs`. The error variants differ (`Subprocess` vs `Build` vs `Io`).
- **The worktree `CARGO_TARGET_DIR` isolation rule is written twice** (`build::cargo_build_observed`, `bench_cmd::cargo_bench`). `docs/commands/bench.md` says it is "the same rule every other brokkr build path applies"; it is only these two call sites. `build::project_info` for a worktree runs metadata without the override, so `ResolvedPaths.target_dir` names the shared target dir, not the one the build uses.
- **Build profile is a bare string.** `BuildConfig.profile: &'static str` is spelled `"release"` in about 14 struct literals plus four near-identical constructors. `resolve_existing_binary` uses it as a directory name, so `"dev"` (from `for_harness(debug=true)`) looks in `target/dev/` instead of `target/debug/`. That is latent: its only caller builds release. It also ignores the worktree target-dir override (the wrong-commit binary bug the other copy warns about). An enum with `dir()` and `cargo_flag()` makes both unrepresentable.
- **`cmd_install` does not use `install_bin_targets`.** It copies the logic and the error string ("in the same words"). The doc says the two "cannot drift", but they agree only by duplication.
- **Lock-context `project_root` is not consistent.** `bench_cmd` says it is "the project root everywhere else". Run, install, deps, clippy and fmt pass `build_root`; check passes `state_root`. `list_smoke` spells the project as `"ratatoskr"` rather than `Project::name()`.
- **Timeout duplicated in text:** `guard::probe_guard` uses `Duration::from_secs(3)` and the literal string `"within 3s"`.
- **Docs restate values and lists the code owns:**
  - check.md's "After 20s … bounded at 120s" (`DRAIN_REAP_AFTER` / `DRAIN_BUDGET`).
  - The list of stamp call sites ("output.rs's three `Command` constructors…").
  - The "~25 cargo call sites" count in `hold.rs` and check.md (should be reworded, not updated).
  - `Cargo.toml` `description` names three projects.
- **Cargo-family comm list lives only in code** (`stray::is_cargo_family`) and is restated in check.md §Strays.

## 2. Values nobody can find, change, or trust

- The tunables in this scope have no index: `DRAIN_BUDGET`, `DRAIN_REAP_AFTER`, the 100ms drain poll, `DEFAULT_KEEP`, `SIGTERM_FORWARD_BUDGET`, the 3s handshake probe, OOM score `1000`, the depth caps 256/64, and `read_lock_contents`' 3×10ms retry. No `brokkr man` section or module lists them.
- **Drain constants have no injection point.** A test of the drain must wait out 20s or 120s. This is the root of the host-reaping test in item 10.
- **`worktree_keep` is read at use time.** Every call site re-resolves `config::hostname()?` and the config is re-detected up to three times per invocation (`parse_cli`, the toolchain arm, the command), each re-parsing `brokkr.toml`. `bench` with no config silently uses `DEFAULT_KEEP`.
- **`RUN_VALUE_FLAGS = ["--features", "-F"]` is maintained by hand.** Adding a value flag to `run` silently breaks the `--` pre-pass. A test comparing it with clap introspection (`Cli::command().find_subcommand("run")` arguments that take values) would enforce it.
- **`bare_run_sentinel` matches the first `"run"` anywhere in argv.** A `run` token that is another command's value (for example, a mogwai target named `run`, or a `--command run` value) followed by `--` would be rewritten. Anchoring on clap's subcommand position is the fix.

## 3. One channel, one implementation

- **`output::lock_msg`, `bench_msg`, `hotpath_msg`, `download_msg`, `verify_summary` and `history_msg` bypass `emit`.** They use a raw `println!`, so they skip the renderer (they can collide with a drawn status line) and the per-run log added in 4db2821. The reap's `SIGKILL sent to …`, drain messages, "lock acquired after …" and all of `guard`/`strays` output never reach the run log. `lock_msg` also ignores quiet mode.
- `lockfile::publish`, `invalidate_mutable_metadata` and `invalidate_metadata` write via `eprintln!("[lock] warning: …")`, a third path.
- **Levels are inconsistent.**
  - Stale-guard and guard-status problems are `lock_msg("WARNING: …")`, not `output::warn`.
  - Non-fatal worktree retention and bookkeeping failures are `output::error`, while the overage is `output::warn`.
  - Toolchain "moved aside" is quiet-suppressible `build_msg`.
- **Silent events.** `worktree_record::save` failures are dropped (`drop(store.save(..))`), as is `toolchain::DisabledToolchain::activate`'s `remove_file(stale).ok()`.
- **Duplicate stderr.** `build::cargo_build_observed` prints stderr through `dump_build_stderr` and then embeds the same stderr in the returned `DevError::Build`.
- **Two `forward_cargo` functions.** `main_parts/commands.rs` (fmt) is a bare `Command::status()` with no `SigtermGuard`, no OOM mark and no child-pid publication. That is the regression `runnables::forward_cargo`'s comment says was fixed for run and install.

## 4. Errors

- `bench_cmd::check_environments` returns `Ok` when either stamp can't be read. Existence was checked, so unreadable or permission failures pass the comparability gate open.
- `worktree_record::Store::load` treats an unparseable file as empty. `save` hand-writes TOML with an unescaped `["{name}"]`, so a name containing `"` corrupts the file, and it is then silently treated as empty. It should use the `toml` crate that is already a dependency.
- `Worktree::create` discards the `git worktree remove` error and falls back to `remove_dir_all` with no dirty check (see item 4).
- **The watchdog `fire()` exits via `std::process::exit`,** so `LockInner::drop` never runs and a `disable_toolchain` pin stays moved aside until the next run in that directory. The toolchain header lists hard kills but not brokkr's own watchdog exit, which is preventable.
- `clean_scratch`'s `kill(pid, 0)` liveness check: EPERM counts as dead, and a recycled PID counts as alive forever, so the directory leaks. It is a bare-PID identity, which the lock code's own rule forbids.
- `watchdog::fire` SIGKILLs descendants by bare PID with no starttime check. `stray::kill`'s `sigkill` closure identity-checks every PID; `kill --hard` uses pidfds. Three signalling policies for one operation.

## 5. Tests that prove nothing

- `hold::tests::stamp_is_a_no_op_without_a_capability` cannot fail. It never inspects `cmd.get_envs()`; it only asserts that `capability()`, if present, is non-empty.
- `stray::tests::own_process_tree_reads_without_panicking` scans the live host's `/proc` and asserts nothing.
- The `watchdog` tests depend on the host's real process tree.
- The lockfile tests depend on the real `$HOME`, the real `compile.lock` and the host's strays (item 10).
- `scripts/guard-smoke.py` flocks the real `~/.brokkr/brokkr.lock`. It is not wired into any `[[script_check]]`, and neither is `guard-decision-probe.py`. check.md says the probe "is worth running after any change"; that is a claim, not a gate. The smoke's three cases are a subset of the probe's, so it looks superseded.
- The crate `build.rs` depends on `git` and `date` on PATH, and `SOURCE_DATE_EPOCH` is not honoured.

## 6. Guards and claims that have stopped holding

- **Reserved env vars are unenforced.** check.md says `BROKKR_HOLD_NONCE`, `BROKKR_COMPILE_LEASE` and `CARGO_CACHE_RUSTC_INFO` must never appear "in a `[[check]] env` block". `RESERVED_SWEEP_ENV` holds only the three rustflags and target-dir keys. Appending `hold::CAPABILITY_ENV`, `LEASE_MARKER_ENV` and `RUSTC_INFO_CACHE_ENV` enforces it in one line.
- **`stamp` coverage is claimed "by construction" but held by convention.** It is true today (`output.rs` ×3, `test_runner`, `commands::forward_cargo`), but more than 40 raw `Command::new` sites exist. A `disallowed_methods` entry for `std::process::Command::new` with allow attributes at the choke points (or a textlint rule with `allow_marker`) would make it structural.
- **`draining` is written and read by nobody.** The `LockState.draining` doc says it is "Published for diagnostics only - `brokkr lock` and the wait message", and check.md says it lets `brokkr lock` and `kill` see who is draining. `parse_lock_contents` ignores it and `LockInfo` has no such field.
- **The toolchain header says activation happens "immediately after taking the flock".** It happens after the drain and capability publication.
- **Re-entrant acquire keeps the outer hold's toolchain arm and context.** A `with_worktree` re-arm under an already-held lock would silently not disable the worktree's pin. That is latent: no such path exists today, and nothing detects it.
- **`find_executable`'s doc says it "falls back to the last executable".** The code errors when there is more than one.
- **`resolve_bin_name` has a dead branch.** It reads `resolve.root`, which is always null under `--no-deps`.
- **`--locked` detection in `cmd_install` checks two guessed paths**, the package dir and the `project_root` argument (which is actually `build_root`). It does not use metadata's `workspace_root`. Invoked from a member subdirectory, it silently installs unlocked, with only an informational line. The comment says it looks "where cargo does".
- **The Idle decision drops the lease.** A compile started while no hold is active never participates, so the drain cannot see it and only the stray reap covers it. The code is consistent with this, but check.md's "admitted compilation holds a reader lease" reads broader than it is.

## 7. Policy invented per call site

- **Child-PID publication is per call site.** `BenchContext::with_build_config` → `cargo_build` passes `on_spawn: None`, so `kill --hard` during a bench build cannot reach cargo. ratatoskr's build path publishes it.
- **Git `short` hashes name worktrees and baselines.** `rev-parse --short` length grows with the repo, so the same commit later gets a new name and the old worktree or baseline is orphaned. The crate `build.rs` alone pins `--short=9`.
- **Ambient dependencies read directly from logic:**
  - wall clock: `worktree_record::now_secs`, lockfile uptime
  - environment: `HOME`, `CARGO_HOME`, `RUSTFLAGS` in `stamp`
  - process-wide cwd: `rustflags::host_triple`
  - global mutable slots: `CAPABILITY`, `DISABLE_DIR`, `HELD`, `SHUTDOWN_REQUESTED`

  All of these are why the tests in item 10 need the host. The capability, toolchain arm and signal state belong on the hold (`LockInner`) rather than in process statics.
- **`nextest-env.toml` is one fixed path,** "safe because holds are serialised". It is also written capability-less outside a hold, so the rationale doesn't cover every writer.
- **`oom::protect_child` applies to every child,** including `git` and `cargo metadata`. The doc says it targets "the benchmark". "degrades on non-Linux" is dead compatibility text for a crate that is Linux-only via `/proc` and `libc` flock.
- **Worktree growth:** the per-project count is acknowledged as a damper only. `enforce` runs a `git status` per candidate on every `--commit` run.

## 8. Code that is no longer load-bearing

- `Record.size_bytes`: nothing writes it.
- `LockState.draining`: nothing reads it.
- `resolve_bin_name`'s `resolve.root` branch: unreachable under `--no-deps`.
- `cfg!(windows)` in `resolve_existing_binary`: the crate cannot build on Windows.
- `output::history_msg` is `#[allow(dead_code)]`.
- `scripts/guard-smoke.py`: superseded by `guard-decision-probe.py`.
- The "`/proc` parse" references in `rustc_guard.rs`: they describe code that was removed.
- `toolchain::DisabledToolchain::activate` is `pub` but reached only through `activate_for_lock` and tests (fmt's direct use was removed). Making it private would enforce the "only via the lock" rule.

## Enforcement summary

| Enforced by | Findings |
|---|---|
| Structural (one owner or one entry point) | reap/warn inside `acquire`; lock before worktree work; shared protocol module across both bins; one cargo-config resolver; one dirty-tree definition; profile enum; refcounted `SigtermGuard`; hold-scoped globals; `install` calling `install_bin_targets` |
| Config parser | reserved env names added to `RESERVED_SWEEP_ENV` |
| Lint or textlint | `disallowed_methods` on `Command::new` and on direct `lockfile::acquire`; `println!`/`eprintln!` in `src/` outside `output.rs` |
| Test | `RUN_VALUE_FLAGS` vs clap; config both-exist precedence; drain with an injectable path and budget; `stamp` asserting on env |
| No mechanical guard available | stale prose claims (items 3–5, 11 and the §6 doc claims); these need rewording to drop drifting specifics, plus the handshake reporting the build hash |
