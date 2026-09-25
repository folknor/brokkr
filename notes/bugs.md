# Bugs

Defects surfaced by the hygiene hunt (eleven read-only scopes). Hygiene findings live in `hygiene-validation.md` (VAL), `hygiene-platform.md` (PLT), `hygiene-measurement.md` (MEA) and `hygiene-projects.md` (PRJ). Nothing here has been verified unless stated; each entry records what the reporting hunter(s) read in the code, and some are explicitly marked "likely" or "inferred" by them.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## BUG-010 - The prose-only shortcut's premise is false for include_str'd docs

Reported by: check-core; the same premise in `git::check_clean`'s comment was reported by measurement-storage.

The shortcut rests on "documentation cannot change how the code builds". brokkr `include_str!`s `docs/**.md` into the binary (`brokkr man`), and any crate with `#![doc = include_str!("README.md")]` has doctests and rustdoc output in markdown, so a docs-only edit skips clippy, rustdoc and the tests that read those files. The `--json` trailer has no field for the shortcut: a markdown-only run reports `verdict: "passed"`. `git::check_clean` excludes `*.md` and `brokkr.toml` on the claim they "don't change what the built binary does"; `brokkr.toml` also carries host features and `capture_env`.

## BUG-011 - Watchdog exit bypasses history, cleanup and toolchain restore; `brokkr clippy` has no ceiling

Reported by: check-core, process-control, config-cli-bootstrap.

`watchdog::fire()` calls `process::exit(124)` from the watchdog thread: `history.db` never records the run (so exactly the runaway runs are missing from `brokkr history`), `RunScope` never drops, the status line is not cleared, and `LockInner::drop` never runs, leaving a `disable_toolchain` pin moved aside until the next run in that directory. `fire` also SIGKILLs descendants by bare PID without the starttime check `stray::kill` applies. `brokkr clippy` arms no ceiling at all, although the ceilings exist because of an observed 1h15m clippy hang.

## BUG-016 - Ctrl-C or `brokkr kill` orphans running tests (remainder)

Reported by: test-execution.

Wave 1 added `src/shutdown.rs` group reaping on SIGINT/SIGTERM and `PR_SET_PDEATHSIG` on the direct child. Remaining: in the serial lanes the direct child is cargo, so a SIGKILLed brokkr can still orphan the test binary under cargo. `check`/`brokkr test` install no `SigtermGuard`, so a graceful `brokkr kill` ends them by the signal rather than as "check interrupted" / exit 130 as `check.md` describes.

## BUG-017 - Child processes spawned with no deadline (remainder)

Reported by: test-execution, convention-engines, ratatoskr, piners, pbfhogg-nidhogg, config-cli-bootstrap.

The `ccu`, piners, `tools.rs` curl and nidhogg curl bullets are fixed. Remaining:

- `brokkr test`'s pre-build and `--timeout` enumeration run through `run_captured_with_env` (deadline `Duration::MAX`); `brokkr test` has no phase watchdog, recreating the build-lock park `IDLE_TIMEOUT` was written for.
- `binary_list` runs test binaries with `--list` under no deadline; its "listing executes no test code" claim ignores ctors and custom harnesses.
- `script_check::run_one` passes `Duration::MAX`: one hung script kills the whole run via the phase watchdog (exit 124) instead of failing that entry.
- ratatoskr `bench_loop` calls `sidecar::run_sidecar` with no deadline and ignores the script's `ceiling:` under `--bench`; a hung harness in a gate sweep holds the global lock indefinitely.
- piners `measured.rs` has no deadline.

## BUG-018 - Most locked commands skip the stray reap and the stale-guard warning

Reported by: process-control, pbfhogg-nidhogg.

Both run only inside `context::acquire_cmd_lock_opt`. At least 12 sites call `lockfile::acquire` directly and get neither: `BenchContext::with_build_config`, `HarnessContext::new`, `BenchHarness::new`, `VerifyHarness::new`/`pbfhogg/verify.rs`, `piners/cmd.rs`, `piners/lint/cmd.rs`, ratatoskr `service_suite`, `service_test`, `saehrimnir`, `list_smoke` (twice), `bench_gate`. CLAUDE.md and `check.md` §Strays say every locked command reaps. (Wave 1 added `piners/registry_io.rs::lock()` for reseed paths - include it.)

Enforcement proposed: move reap and warning into `lockfile::acquire`'s fresh-hold branch and make it private to that path, or a textlint ban on `lockfile::acquire(` outside one file.

## BUG-019 - Cargo builds run without the lock

Reported by: elivagar, pbfhogg-nidhogg.

- `elivagar/dispatch.rs::run_elivagar_run` (`BuildKind::Example`) calls `cargo_build` with no lock (its own comment admits it), and `run_measured` does not take one.
- nidhogg `verify readonly` runs `cargo_build` with no lock; `bootstrap.rs` takes none for the nidhogg verify variants.
- A guarded host refuses these builds; an unguarded one builds concurrently. Enforcement proposed: `build::cargo_build` takes `&LockGuard`.

## BUG-027 - Lockfile unit tests touch the real compile lock and can SIGKILL host processes

Reported by: process-control.

The `lockfile.rs` tests claim they never touch the real global lock, but `acquire_at` → `drain_and_authorize` → `drain_compile_leases` uses the real `~/.brokkr/compile.lock`, and after 20s calls `stray::reap_for_drain()`, which can SIGKILL a real rust-analyzer cargo (20s is also the per-test cap). The tests mutate process-global state without the serialising lock: `hold::publish_capability`/`clear_capability` (the hold test claims serialization the lockfile tests don't take) and `toolchain::DISABLE_DIR` (armed by a toolchain test while lockfile tests call `activate_for_lock`, a cross-module flake that can rename files in the other test's scratch). `acquire_at`, the "unit-test seam", runs the global drain, reap and capability publication.

## BUG-029 - Bench builds don't publish the child PID

Reported by: process-control.

`BenchContext::with_build_config` → `cargo_build` passes `on_spawn: None`, so `kill --hard` during a bench build cannot reach cargo; ratatoskr's build path publishes it.

## BUG-036 - `DevError::Subprocess` renders spawn failures as signal deaths (remainder)

Reported by: config-cli-bootstrap.

Wave 1 added `DevError::Spawn` and made `Subprocess { code: None }` render neutrally. Remaining: ~15 spawn/wait sites still build `Subprocess { code: None }` from an `io::Error` (`test_runner.rs` ×4, `nidhogg/*`, `worktree.rs`, `git.rs`, `wc.rs`, `gremlins.rs`, `ratatoskr/saehrimnir.rs`, `nidhogg/bench_tiles.rs`, `pbfhogg/verify_merge.rs`) and should use `Spawn`; `pbfhogg/verify.rs::check_exit` omits the signal number.

## BUG-041 - `BenchConfig.mode` and `brokkr_args` are never set; mode is never printed or stored

Reported by: measurement-storage.

All ~35 construction sites set `None`; `format_result_line` and `build_run_info` read `config.mode` rather than the harness's `measure_mode`, so `[result]` never prints `mode=` and `sidecar_meta.mode` is NULL for every run (the `brokkr sidecar` "mode:" line never appears).

## BUG-042 - `brokkr lock`'s "last marker" is blind for `--hotpath`/`--alloc` and sync bench

Reported by: measurement-storage.

`SidecarFifo::status_path` writes `.sidecar-status` beside the FIFO (`with_file_name`); `run_hotpath_capture` gets `paths.scratch_dir`. The reader looks at `project_root/.brokkr/.sidecar-status`. The ratatoskr bench gate has the same mismatch. It fails silently.

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

## BUG-055 - Commit identity width has no owner

Reported by: measurement-storage, process-control, elivagar.

`git rev-parse --short` length grows with the repo: a longer hash passed to `--commit`/`--compare` matches nothing; the same commit later gets a new worktree/baseline/archive name and the old one is orphaned; a user hash of a different length fails with "no build". Only the crate `build.rs` pins `--short=9`.

## BUG-065 - External-baseline benches measure or detect the wrong thing (remainder)

Reported by: elivagar.

Wave 1 fixed the timed `--download`, priming detection, dangling tilemaker symlinks, partial-unzip detection and `bench_all`'s "skipped" downgrade. Remaining: downloads get no hash check (osmdata regenerates daily; JDK/JAR downloads live in `tools.rs`). The planetiler `--download_dir` flag name was written from memory and is unverified against planetiler.

## BUG-069 - SIGTERM during sync bench orphans the mock (remainder)

Reported by: ratatoskr.

Wave 1 added a `PhaseGuard` covering every phase except each measured iteration. Two small windows remain (harness fork/exec, and between the guard's last check and its drop); closing them needs a re-entrant `SigtermGuard` (PLT-012).

## BUG-101 - Three writers of `results.db`'s `user_version` (remainder)

Reported by: small-benches.

litehtml's `MechanicalDb` no longer reads or writes `user_version`. Remaining: a legacy file litehtml already stamped `user_version = 1` keeps that stamp, so `ResultsDb`'s `migrate_uuid` may be skipped on it; `ResultsDb` should detect that case.

## BUG-104 - Dry-run output is invisible by default for dellingr, mogwai and sluggrs hotpath

Reported by: small-benches.

`[dry-run]` lines go through quiet-gated `bench_msg` and `run_measured` sets quiet unless `--verbose`; pbfhogg/elivagar use `run_msg`. `hotpath.md` says `--dry-run` "prints them". The mogwai bare index prints its header via gated `bench_msg` and its body via raw `print!`, so without `-v` the header is missing.

## BUG-117 - Remaining non-atomic replace-by-rename copies

Reported by: wave-1 integrator.

`src/atomic_write.rs::replace` is now the shared helper (piners registries, pbfhogg dataset TOML, `worktrees.toml`). `elivagar/corpus/digest.rs::write_atomic` is a weaker copy (no fsync, fixed `.tmp` name, so two writers collide), and `tools.rs` plus the `preflight.rs` hash-cache save each hand-roll temp-file-and-rename.

## BUG-115 - pbfhogg dispatch panics if a command stops mapping through `as_pbfhogg`

Reported by: config-cli-bootstrap, pbfhogg-nidhogg.

The `unreachable!()` block in `main_parts/bootstrap.rs` covers every pbfhogg command and holds only while `as_pbfhogg()` returns `Some` for each; `MultiExtract` already moved out once. `pbfhogg/cmd.rs::verify` has `unreachable!()` for elivagar/nidhogg variants relying on caller match order; `bench_extract::strategy_args` and `bench_blob_filter::command_args` have `unreachable!()` on an unknown name. Proposed: exhaustive match over a pbfhogg sub-enum.
