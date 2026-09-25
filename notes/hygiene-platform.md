# Hygiene - platform and cross-cutting

Hygiene findings from the hunt for process control (lock, hold, guard, strays, shutdown, toolchain, worktrees, builds, runnables, `bench`), config/CLI/bootstrap (config parsing, CLI schema, project detection, output, errors, history, tools, preflight, env, man), and every theme that recurred across scopes. Where a theme has sites inside project code, they are listed here rather than in `hygiene-projects.md`. Siblings: `hygiene-validation.md` (VAL), `hygiene-measurement.md` (MEA), `hygiene-projects.md` (PRJ). Entries record what hunters reported; nothing has been verified.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## PLT-001 - The `.brokkr` state root is spelled at ~90 sites

Reported by: config-cli-bootstrap, test-execution, measurement-storage, ratatoskr, piners, small-benches, pbfhogg-nidhogg.

`resolve::results_db_path` and similar exist, but paths are hand-built at ~90 sites in ~40 files (test-execution counted ~25 `join(".brokkr")`; measurement ~20). Examples: `commands.rs` builds `.brokkr/ratatoskr`, `.brokkr/dellingr`, `.brokkr/piners/corpus`, `.brokkr/.sidecar-status`; `preflight` builds `.brokkr/hash_cache`; test-hung, parallel-timings, nextest-synth; `BenchHarness::new_with_lock` and `store_sidecar` rebuild the results/sidecar DB paths (`output-channels.md` claims both are "resolved identically … by `BenchHarness::new_with_lock`"); ratatoskr `ARTEFACT_PARENT`, `MOCK_DIR`, `SYNC_ARTEFACT_PARENT`, the gate.db path; piners `ARTEFACT_PARENT`+`"corpus"`, `resolve_parts/runtime.rs::corpus_runs_db_path`, a literal in `clean_artefact_trees`; `dellingr/cmd.rs::SCRATCH_REL` and `.brokkr/mogwai`; nidhogg `.brokkr/nidhogg.pid` (twice). Project-level layouts: PRJ-033 (ratatoskr), PRJ-043 (piners), PRJ-055 (dellingr/mogwai).

Enforcement proposed: a `StateDir` type with named accessors (and per-project layout types), held by a textlint rule forbidding `".brokkr` / `join(".brokkr")` outside it.

## PLT-002 - brokkr enforces very little of its own convention on itself

Reported by: every hunter (check-core, process-control, config-cli-bootstrap, measurement-storage, pbfhogg-nidhogg, ratatoskr, piners, small-benches).

The repo's `brokkr.toml` has three `[[textlint]]` rules (`docs-cite-no-line-numbers`, `docs-never-cite-notes`, `comments-never-cite-notes`) plus `[rustdoc]`; no `[[script_check]]`, `[[dependency_rule]]`, `[header]`, `[gremlins]`, `[lints]`, `[manifest]` or `[[check]]` entries, so its own gate runs the legacy `--all-features` fallback. No `clippy.toml`, so `disallowed_methods`/`disallowed_macros` are unused. `Cargo.toml [lints.clippy]` denies ~30 generic lints including `unwrap_used`, but not `expect_used`, `panic`, `indexing_slicing`, `print_stdout`, `print_stderr` or `exit`. `too_many_arguments` is denied but waived at ~15 pbfhogg/nidhogg sites and five ratatoskr sites (`too_many_lines` four times), `#[allow(clippy::too_many_arguments)]` sits on one-argument `HistoryDb::insert`. Nothing enforces `fmt` (piners `measured.rs` has an unformatted line). The same 15-lint `#![allow(...)]` block is pasted into every test module, including lints that aren't denied.

Hunters' collective view: nearly every consolidation across all four hygiene files could be held by textlint, `clippy.toml`, enums, or a parse-time refusal the build already supports. `[[dependency_rule]]` can't express intra-crate boundaries (brokkr is a single crate), so module boundaries need textlint.

## PLT-003 - HOME/XDG/CARGO_HOME resolution is re-implemented per site

Reported by: config-cli-bootstrap, measurement-storage, process-control, convention-engines.

- `XDG_CONFIG_HOME`: `config_parts/user.rs::user_config_path`. `XDG_DATA_HOME`: `history.rs::db_path` (`var`), `history_cmd::record_history` (restates the rule as a precondition using `var_os`; disagrees on non-UTF-8), `format_sidecar.rs::sidecar_backup_dir`. None treats empty as unset (BUG-032).
- `$HOME/.brokkr`: `lockfile::lock_path` (`var("HOME")`, errors on unset/non-UTF-8), `hold::cargo_config_overrides` (`var`), `rustc_guard::brokkr_dir` and `lock_is_held` (a second copy in the same file, `var_os`). Empty `HOME` gives a relative `.brokkr/brokkr.lock` in brokkr's cwd and in rustc's cwd for the guard, so the two sides lock different files; nothing rejects empty.
- `HOME` also in `guard`, `deps/focus::tildify`; `CARGO_HOME` → `$HOME/.cargo` in `bench_cmd/stamp`, `rustflags::config_paths`, `guard` (three times; see BUG-024).

Enforcement proposed: one `dirs` module plus `disallowed-methods` on `std::env::var`/`var_os` elsewhere (or a textlint ban on `env::var("XDG_`). `rustc_guard` is a separate bin and may need a documented copy (see PLT-011).

## PLT-004 - Output bypasses the output channel

Reported by: config-cli-bootstrap, process-control, measurement-storage, ratatoskr, piners, test-execution, pbfhogg-nidhogg, elivagar, convention-engines.

- `output.rs` claims "All output goes to stdout (stderr reserved for panics only)" and "every prefixed printer has to go through [the renderer]". Only `emit()` does; `lock_msg`, `bench_msg`, `hotpath_msg`, `verify_msg`, `download_msg`, `verify_summary`, `history_msg` and the per-project `ratatoskr_msg`, `corpus_msg`, `lint_msg`, `sluggrs_msg` etc. `println!` directly, skipping the run log added in 4db2821, status-line clearing and (for `lock_msg`) quiet mode. So the reap's "SIGKILL sent to …", drain messages, "lock acquired after …", all `guard`/`strays` output, ratatoskr and piners output never reach the run log.
- stderr writes: `sidecar_msg`, `record_history` (`eprintln!("[history] warning: ...")`), `man::write_stdout`, `tools::download_file`, `lockfile::publish`/`invalidate_mutable_metadata`/`invalidate_metadata` (`eprintln!("[lock] warning: …")`), `print_run_info` (`eprintln!("[sidecar] …")`, `eprintln!("[error] …")`), nidhogg `ingest.rs`/`update.rs` (`eprint!`/`print!`, captured then dumped so "progress" appears only after exit). "stored in results.db" (stdout) and "stored in sidecar.db" (stderr) land on different streams.
- `[result]` has two owners: `emit_result_lines` via `output::result_msg`, `force_emit_result_lines` raw `println!`; `record_result` also `println!("{short}")`.
- ~220 direct `println!`/`eprintln!` outside `output.rs` (sidecar_fmt 46, test_cmd 21, env 14, …); some are legitimate data output (deps JSON, `outline`, query tables in `corpus_query.rs`/`lint/query.rs`).
- `inspect.rs`, `diag.rs`, `svg.rs`, `bench_pmtiles.rs`, `bench_node_store.rs` each hand-roll capture → `print!`/`eprint!` → `check_success`; their headers claim output is "streamed directly".
- `compare_tiles` mixes `bench_msg` census lines with raw `println!`; `pmtiles::run` prints errors with `println!`.

Enforcement proposed: clippy `print_stdout`/`print_stderr` or `disallowed-macros` with a per-file `#[allow]` for data printers; or a textlint rule `println!\("\[` outside `src/output.rs`.

## PLT-005 - Operator text formatting is invented per site

Reported by: config-cli-bootstrap, ratatoskr, piners, check-core, process-control, measurement-storage, small-benches, elivagar.

- The "10-char prefix column" is broken by `[download] `, `[litehtml] `, `[sluggrs] `, `[ratatoskr] `, `[sidecar] `, `[history] `. `artefacts::emit_clean_hint` hard-codes `[ratatoskr]` in a module shared with piners.
- Warnings written as fake tags or plain text: ratatoskr `"  [warn] ..."` five times plus inside `missing_baseline_error`; piners `corpus_msg("warning: ...")` in `report.rs`, `bless.rs`, `cmd.rs`; stale-guard and guard-status problems as `lock_msg("WARNING: …")` rather than `output::warn`; no warn-level rule (record_history, man, tools each pick their own).
- Levels inconsistent: non-fatal worktree retention/bookkeeping failures are `output::error` while the overage is `output::warn`; toolchain "moved aside" is quiet-suppressible `build_msg`; `print_run_info`'s `[error]` goes to stderr while `output::error` goes to stdout.
- `(s)` plural hedges throughout ratatoskr ("gate(s)", "script(s)", "iteration(s)", "row(s)") and piners although `output::count` exists; `service_suite` hand-rolls plurals.
- One flow, several prefixes: the ratatoskr bench loop mixes `[bench]` and `[ratatoskr]`; the build line is `[harness]`; tools.rs reports downloads via `verify_msg` (osmosis) and `bench_msg` (JDK/planetiler) while `download_msg` goes unused.
- The "NOTE: alloc profiling" banner exists eight times and has diverged (`--` vs `-` in nidhogg/elivagar).
- Duration formatting: `fmt_wall` vs `lockfile::format_duration` (the latter used by the watchdog); service prints `412ms`, sync `0.4s`.

Enforcement proposed: a prefix table with a width test; textlint bans on `\[warn\]` literals outside `output.rs` and `\(s\)` in string regions; one banner constant.

## PLT-006 - Subprocess spawning bypasses the choke points

Reported by: process-control, config-cli-bootstrap, ratatoskr, pbfhogg-nidhogg, convention-engines, elivagar.

`hold::stamp` claims coverage of "every child brokkr starts under a hold, by construction", but that holds only for the choke points (`output.rs` ×3, `test_runner`, `commands::forward_cargo`); there are >40 raw `Command::new` sites. Examples: `ccu`, `gremlins::tracked_files`, `scope::git_paths`, `rustflags::host_triple` (convention-engines); `env::read_tool_version`/`read_git_rev`, `tools::download_file`/`head_url`/`check_curl`, `preflight::check_binary`, `wc::rust_files` (config-cli); sæhrimnir's `spawn_observed` and `run_mock_serve` (no stamp, no `oom::protect_child`); 13 pbfhogg/nidhogg sites (nidhogg server and curl in `bench_tiles.rs`, server and `pkill` in `server.rs`, curl in `client.rs` ×3 and `bench_api.rs` ×2, `chmod` in `verify_readonly.rs`, osmosis in `verify_merge.rs::run_osmosis` which rebuilds `CapturedOutput` by hand, `which` in `verify.rs`). Missing: `hold::stamp`, `oom::protect_child`, shutdown poll, child-PID publication (BUG-029). `main_parts/commands.rs::forward_cargo` (fmt) is a bare `Command::status()` with no `SigtermGuard`, OOM mark or PID publication - the regression `runnables::forward_cargo`'s comment says was fixed for run/install; two `forward_cargo` functions exist. `run_passthrough_in`'s own doc names exactly this failure. `oom::protect_child` applies to every child including `git` and `cargo metadata`, while its doc says it targets "the benchmark".

Enforcement proposed: `clippy.toml` `disallowed-methods = ["std::process::Command::new"]` with `#[allow]` only at the choke-point module.

## PLT-007 - Errors lose their subject or carry the wrong class

Reported by: config-cli-bootstrap, convention-engines, measurement-storage, ratatoskr, pbfhogg-nidhogg, elivagar, check-core.

- `From<io::Error>` gives `io: No such file or directory` naming no path (`HistoryDb::open` `create_dir_all`, `cmd_run` `create_dir_all(scratch)`, `resolve::file_size_mb`, `preflight::cached_xxh128`, `wc`); ratatoskr "canonicalize script: {e}" (and `run_sync_bench` canonicalizes before its `is_file` check, so the friendlier message is unreachable); pbfhogg "failed to remove cached merged PBF: {e}", "failed to create scratch dir: {e}" (also as `Config`).
- `From<toml::de::Error>` in `config::load` never says which `brokkr.toml` (cwd or parent); the user layer wraps with its path.
- `From<serde_json::Error>` → `DevError::Build` (adoptium API JSON, cargo metadata schema mismatches: VAL-038); `hostname()` failure → `Config`.
- Runtime failures as `Config`: ratatoskr "harness binary exited", "gate FAILED", readiness timeouts, "no successful iterations" (users see `config: gate ... FAILED`; `sync.md` shows it), while service returns `ExitCode(1)`; pbfhogg `verify_geocode`/`verify_readonly` failures and `ensure_merged_pbf`/`bench_*` IO failures as `Config` while `verify_batch` uses `Verify`.
- `Verify` for non-verify failures: `run_variants` wraps benchmark failures, `compare_tiles` plain IO, check test timeouts.
- `brokkr deps nosuchcrate` → `Build`.
- piners: every non-`Interrupted` error from `run_captured_with_env_and_deadline` recorded as "harness failed to spawn", and that branch never calls `clear_child_pid`.
- UTF-8 path helpers: five variants with messages that disagree, some not naming the path (PRJ-010).

Enforcement proposed: remove the blanket `From<io::Error>` to force context; a `Spawn` variant (BUG-036).

## PLT-008 - Errors swallowed into defaults (platform code)

Reported by: config-cli-bootstrap, process-control.

`Cleaner::dir`/`file` count a path as "removed" whatever `remove_*` returned; `HistoryDb::query` uses `.filter_map(Result::ok)`; `tools` `git pull --ff-only` failures dropped ("tolerate"); preflight hash-cache write best-effort; `worktree_record::save` failures dropped; `toolchain::DisabledToolchain::activate` `remove_file(stale).ok()`; `host_features` swallows a `hostname()` error other callers propagate; `build::cargo_build_observed` prints stderr via `dump_build_stderr` and then embeds the same stderr in `DevError::Build` (duplicate, not swallowed). Measurement stores: MEA-013; per-project: PRJ-013, PRJ-026, PRJ-037, PRJ-048.

## PLT-009 - Panics on input an operator or file controls

Reported by: elivagar, pbfhogg-nidhogg, piners, measurement-storage, ratatoskr.

Internal `expect`/`unreachable!` in production: elivagar `engine.rs`, `compare.rs`, `overlay.rs`, `pairing.rs`, `mutate.rs`, `dispatch.rs`; pbfhogg (BUG-115); piners `measured.rs` `MeasureMode::Run => unreachable!` for a dispatch contract (refuses `--bench` at runtime instead of clap conflicts); `sidecar_cmd` `.expect` on a clap guarantee and `run_compare` indexing `uuids[0]`/`[1]`; ratatoskr `run_gate_hook`'s `.expect("gate existence already validated upstream")` depending on call order. Allowed only because `expect_used`/`indexing_slicing` aren't denied (PLT-002). PMTiles decoding panics: BUG-062.

## PLT-010 - `/proc` parsing, process-tree walks and signalling are re-implemented; shared code depends on `ratatoskr::process`

Reported by: test-execution, process-control, ratatoskr, measurement-storage, config-cli-bootstrap, pbfhogg-nidhogg.

- `/proc/<pid>/stat` parsed four times: `lockfile::proc_starttime`, `stray::read_proc`, `sidecar.rs`, `check_cmd/watchdog.rs::proc_ppid`; `test_runner` walks `status` PPid and `task/children`; tree walks in `stray::collect_descendants`, `watchdog::descendants`, `ratatoskr/process.rs`.
- Group signalling in `test_runner::signal_process_group` (duplicates `send_signal_pgrp`), `ratatoskr/process.rs`, `main_parts/commands.rs`; seven other raw `libc::kill` sites.
- Three signalling policies for one operation: `watchdog::fire` by bare PID; `stray::kill` identity-checks each PID; `kill --hard` uses pidfds. Liveness: BUG-040.
- `output.rs`, `sidecar.rs`, `test_runner.rs` (`ratatoskr::process::snapshot_proc`) and `nidhogg/bench_tiles.rs` (`send_signal`) import from `crate::ratatoskr::process` - a generic dependency pointing into a project module.

Enforcement proposed: move `process.rs` to a shared module; textlint ban on `crate::ratatoskr` outside `src/ratatoskr*` (also covers piners' `crate::ratatoskr::build`, PRJ-051).

## PLT-011 - Lock-protocol literals are copied across the bin boundary

Reported by: process-control.

`rustc_guard.rs` hand-spells `"BROKKR_HOLD_NONCE"`, `"BROKKR_COMPILE_LEASE"`, `"BROKKR_CARGO"`, `".brokkr"`, `"brokkr.lock"`, `"compile.lock"`, the `auth=` key, `auth_hash`, and handshake `protocol=1`, pairing with `hold.rs` constants, `lockfile.rs` paths and `guard::GUARD_PROTOCOL`. The stated reason ("no lib target") doesn't force the copy: a `#[path = "../lock_protocol.rs"] mod protocol;` included by both bins would share them. Hunter's further suggestion: `BROKKR_LONG_VERSION` applies to both bins, so the guard could print its build hash for an exact comparison, replacing the hand-bumped protocol number. `guard::probe_guard` uses `Duration::from_secs(3)` and the literal `"within 3s"`.

## PLT-012 - `SigtermGuard` is not re-entrant; interrupt policy differs per caller

Reported by: process-control, ratatoskr.

`install` resets `SHUTDOWN_REQUESTED`; `Drop` restores `SIG_DFL` and clears the flag. Outer guards exist in ratatoskr, piners and `list_smoke`; inner guards in `run_passthrough_in` and `run_sidecar`. Nesting silently drops a pending request, and after the inner guard drops, Ctrl-C kills brokkr outright with no mock teardown or toolchain restore (process-control found no live nesting path; ratatoskr's BUG-067 is the flag reset in practice). Because the guard can't nest, the ratatoskr bench path runs unguarded (BUG-069). Interrupt handling per cohort: stop (service), continue (sync --all, BUG-066), continue with flag reset (gate sweep, BUG-067). `MockServeSignalGuard` is a second signal-flag implementation. About a dozen call sites carry `true, // isolate_pg: caller's SigtermGuard active`, unchecked. The lock solved the same nesting with refcounting.

Enforcement proposed: a refcounted guard; a signature like `Isolation::Pg(&SigtermGuard)`.

## PLT-013 - Hold-scoped state lives in process globals

Reported by: process-control.

`CAPABILITY`, `DISABLE_DIR`, `HELD`, `SHUTDOWN_REQUESTED` are process statics; production and tests share every global (BUG-027). Re-entrant acquire keeps the outer hold's toolchain arm and context, so a `with_worktree` re-arm under an already-held lock would silently not disable the worktree's pin (latent, undetected). Drain constants (`DRAIN_BUDGET`, `DRAIN_REAP_AFTER`) have no injection point, so a drain test waits 20s or 120s. `toolchain::DisabledToolchain::activate` is `pub` but reached only via `activate_for_lock` and tests; making it private would enforce "only via the lock". Hunter's view: capability, toolchain arm and signal state belong on the hold (`LockInner`).

## PLT-014 - Reserved env vars are documented, not enforced

Reported by: process-control.

check.md says `BROKKR_HOLD_NONCE`, `BROKKR_COMPILE_LEASE` and `CARGO_CACHE_RUSTC_INFO` must never appear in a `[[check]] env` block; `RESERVED_SWEEP_ENV` holds only `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, `CARGO_TARGET_DIR`. Appending `hold::CAPABILITY_ENV`, `LEASE_MARKER_ENV`, `RUSTC_INFO_CACHE_ENV` enforces it in one line.

## PLT-015 - `cargo metadata` is spawned and deserialized many times

Reported by: convention-engines, check-core, process-control.

- Invoked from 8+ modules with their own deserializers: `deps::CargoMetadata`, `dependency_rules::CargoMetadata`, the `Value`-based one in `build.rs`, runnables, bench, direct_runtime, nextest; spawn-and-parse block copied in `build::project_info`, `build::resolve_existing_binary`, `bench_cmd/discover.rs`, `runnables::discover`, `deps`, each with its own error text. `deps/mod.rs::run_metadata` and `dependency_rules::check` are the same function twice, down to the error string.
- Deserializers already differ: `dependency_rules` requires `optional`; `deps` defaults every optional field.
- Up to ~8 runs per `check` (`verify_doc_only_rules`, `resolve_sweep_unification`, clippy, rustdoc, test phase, coverage, dependency_rules, publish_cycle - the last two back to back with `--no-deps`); no single `ProjectInfo` per run.
- `resolve_bin_name` reads `resolve.root`, always null under `--no-deps` (dead branch).

Enforcement proposed: one metadata module computed once per run, plus a textlint ban on a `"metadata"` argv outside it.

## PLT-016 - Git invocations and the tracked-file walk have many owners

Reported by: convention-engines, process-control, config-cli-bootstrap.

`gremlins::tracked_files` is the file list for header, textlint and manifest (which is why they import `gremlins`); `scope.rs` has its own `git_paths`/`ls-files`; `wc::rust_files` duplicates gremlins' `git ls-files` byte for byte. One `check` can run `git ls-files` up to five times, and the walks treat non-UTF-8 paths oppositely (VAL-040). "Run git, trim stdout" helpers: `worktree::run_git`, `worktree_record::is_dirty`, `bench_cmd::git` (via `output`), `git.rs` (three), `scope.rs`, `wc.rs`, `gremlins.rs`, with differing error variants (`Subprocess`/`Build`/`Io`).

Enforcement proposed: one corpus module computed once per run and passed to every engine; a rule forbidding engines from spawning `git`.

## PLT-017 - "Dirty tree" has several definitions

Reported by: process-control, measurement-storage, small-benches.

`bench_cmd::is_dirty` (raw porcelain: untracked and `.brokkr/` count) vs `git::check_clean` (excludes `.brokkr/`, `*.md`, `brokkr.toml`, `approved.png`, the toolchain sidecar - written after tracked `gate.db` writes blocked the next run; `bench` never got that fix, and its own baselines under `.brokkr/bench/` dirty the tree unless gitignored); `worktree_record::is_dirty` (porcelain; failure counts as dirty); `scope::dirt`; crate `build.rs` (porcelain, so this repo's build is `-dirty` today because of untracked notes). `git::check_clean` turns git failures into "dirty tree". The `snapshots/*/approved.png` pathspec in `check_clean` silently stops matching if sluggrs moves the directory. The check_clean comment's premise is false (BUG-010). ratatoskr runs the dirty check once per gate inside `--gate all`, so a dirty tree gives one identical failure per gate.

Enforcement proposed: one definition parameterised by question ("honest commit" vs "would deletion destroy work").

## PLT-018 - Host triple and `rustc -vV` are parsed in several places

Reported by: convention-engines, process-control.

`deps::host_triple` (fallible, cwd `"."` not project root), `rustflags::host_triple` (`OnceLock`, raw `Command`, cwd; can report a different toolchain's host), `bench_cmd/stamp.rs::rustc_version` (in `build_root`). Enforcement proposed: one owner; textlint forbidding `"-vV"` elsewhere.

## PLT-019 - Build profile is a bare string or bool

Reported by: process-control, ratatoskr, test-execution, piners.

`BuildConfig.profile: &'static str` spelled `"release"` in ~14 struct literals and four near-identical constructors; `resolve_existing_binary` uses it as a directory name, so `"dev"` (from `for_harness(debug=true)`) looks in `target/dev/` (latent; only caller builds release) and it ignores the worktree target-dir override. ratatoskr has three spellings for one `debug: bool` (`"dev"` in `ratatoskr/build.rs` and `for_harness`, `"debug"` in the gate row, hard-coded `Release` in results.db); `CargoProfile` has no dev variant (BUG-047). `test_cmd` uses a bool (VAL-005). The `ratatoskr/build.rs` header says it is ratatoskr-only, reads `[ratatoskr.harness]` and defaults to release; piners and lint call it with debug defaults.

Enforcement proposed: one enum with `dir()`, `cargo_flag()`, `as_label()`.

## PLT-020 - `clean` breaks its constructed-name rule and misses brokkr's own leftovers

Reported by: config-cli-bootstrap, elivagar, pbfhogg-nidhogg, ratatoskr, piners, small-benches, measurement-storage, test-execution.

`docs/commands/clean.md` says clean removes only brokkr-designated or constructed names and never parses names back. Against that:
- It deletes other tools' tmp names: `ocean-build_tmp` (elivagar's default, never passed by brokkr), `.ingest_tmp`/`.tilegen_tmp` in the nidhogg arm (never created by the nidhogg module), `.pbfhogg-external-join-<pid>` (parsed, then checked with a bare `kill(pid,0)`, BUG-040).
- It prefix-matches `geocode-*`, sweeps every `*.pbf`, finds piners run dirs by `starts_with("run-")`, and elivagar's deep clean deletes every `*.pmtiles` (BUG-064).
- ratatoskr's `gate.db` is spared only because clean keeps every file directly under `.brokkr/ratatoskr` and deletes every directory; moving `gate.db` into a subdirectory would delete baselines silently.
- Misses: the nidhogg `*-ingest-output` dirs; pbfhogg `*.osc.gz` scratch and `multi-extract/`; `.brokkr/mogwai` scratch; `.brokkr/test-hung/`; `.sidecar-<pid>.fifo`/`.sidecar-status` after SIGKILL (a stale status can show a dead run's marker); `.brokkr/capture.js`, `.brokkr/prepare-cache`; the piners lint directory (`lint-corpus.md` claims "`brokkr clean` spares it"); `$XDG_DATA_HOME/brokkr/sidecar-backups/` is missing from clean.md's store table.
- Wrong dir: BUG-035.
- Tilegen retention has two pruners with identical read_dir → mtime-sort → skip logic: `elivagar/dispatch.rs::prune_output_dir` (`OUTPUT_RETENTION = 5`, not configurable) and `commands.rs::clean_archives` (`--keep` default 2).

Enforcement proposed: layout types that clean asks for removable names (with a test that clean spares `layout.gate_db()`), one `Project::scratch_rel()` read by writers and clean.

## PLT-021 - `brokkr.toml` is parsed three to five times per invocation

Reported by: config-cli-bootstrap, process-control.

Call sites: `parse_cli`, the `disable_dir` peek, per-command `detect_optional()`, `project::detect()`, `record_history` at exit (after a run that may last 25 minutes), `bench_cmd` internally. Each reloads the user layer and re-runs `Box::leak` for `Other`. `project::detect` ("read and parsed exactly once") and `Project::Other` ("leaked once at startup") are false today. `worktree_keep(&config::hostname()?)` and the `cwd != project_root` → `parent_build_root` derivation repeat three times in `bootstrap.rs`, though `Detection.build_root` carries the value; `bench` with no config silently uses `DEFAULT_KEEP`.

Enforcement proposed: detect once in `main` and thread a `Detection`; a `OnceLock` cache in `project` makes a second parse impossible.

## PLT-022 - The top-level config key universe has three owners

Reported by: config-cli-bootstrap.

`load()` calls each `parse_*`; `parse_hosts` has its own skip list of 24 keys; `docs/brokkr.toml.md` "Reserved top-level keys" lists 16 and lacks `ratatoskr`, `piners`, `dellingr`, `mogwai`, `quarantine`, `clippy`, `lints`, `bin`. A host named `test`, `check` or `bin` cannot be configured.

Enforcement proposed: one `SECTIONS: &[(&str, parser)]` table driving parsing and the host skip, plus a doc-parity test (or the doc stops restating it).

## PLT-023 - "Which command runs in which project" has two owners

Reported by: config-cli-bootstrap, small-benches, elivagar.

`cli/visibility.rs TABLE` ("must be kept in agreement with those call sites") vs scattered handler checks: `project::require`, the `cmd_pmtiles_stats` inline gate, the `'X' is only available for litehtml/sluggrs` match arms. Only pmtiles-stats has an agreement test, and it re-lists the set by hand instead of reading `TABLE`. Two message shapes ("in X projects" / "for litehtml/sluggrs projects"). `visibility.rs` says each handler's `require` is "the authoritative refusal", but for `visual`, `list`, `report`, `visual-status` the dispatcher's `match project` is the gate and the inner `require` never fires; `hotpath` is required twice; piners `measured.rs` repeats dispatch's `require`; elivagar's `require` runs after the lock is taken. `TABLE` claims "Sorted by name" and isn't (mogwai, install, guard/strays/wc); duplicates untested. `require` labels name nonexistent commands (BUG-105).

Enforcement proposed: make `TABLE` the gate in dispatch and delete per-handler checks; test sortedness and uniqueness.

## PLT-024 - Project name ↔ variant mapping is duplicated

Reported by: config-cli-bootstrap, ratatoskr, pbfhogg-nidhogg.

`config::load`'s string match and `Project::name()`; hand-written `Project` lists in tests miss `Saehrimnir` and `Mogwai`. Literal project names instead of `Project::name()`: ratatoskr `LockContext { project: "ratatoskr", ... }` at six sites, `list_smoke`, pbfhogg `VerifyHarness` (`"pbfhogg"`), measurement's legacy default `'pbfhogg'` (MEA-005). `Project::cli_package()` owns package names yet they are restated (PRJ-001).

Enforcement proposed: `Project::ALL` and a round-trip test.

## PLT-025 - Configuration is validated at use instead of at load (platform config)

Reported by: config-cli-bootstrap, process-control.

- `worktree_keep = 0` silently remapped to the default at use (`DevConfig::worktree_keep`), while `parallel.budget = 0` is refused at parse.
- `DriveConfig` lacks `deny_unknown_fields` (every sibling denies), so typos under `drives.*` are ignored.
- A misspelled hostname section silently resolves to default paths and no host features (`resolve_paths`, `host_features`). Checkable: warn when `hosts` is non-empty and none matches.
- `QuarantineEntry.category` is a free string.
- Stringly config in other areas: VAL-035 (convention engines), PRJ-011 (pbfhogg), PRJ-024 (elivagar), PRJ-034 (ratatoskr), PRJ-046 (piners), PRJ-056 (litehtml).
- `captured_env_pairs` reads `std::env::vars()` itself (no injection point).

## PLT-026 - CLI values are parsed or checked twice

Reported by: config-cli-bootstrap, measurement-storage, pbfhogg-nidhogg.

- `--osc-range`: `validate_osc_range`, then `resolve::resolve_osc_range`.
- `--compression`: `validate_compression`, then `pbfhogg/mod.rs` (the two disagree: PRJ-006).
- `--bench/--hotpath/--alloc` exclusivity and `runs >= 1` in `resolve_mode` after parse, `--runs >= 1` again in `cmd_run`, and "benchmark requires at least 1 run" re-raised in four harness loops; bench_gate's `--bench 0` gets a third message (MEA-003).
- `validate_meta_filter`/`validate_env_kv` claim "exactly one `=`" and don't check it.

Enforcement proposed: clap value parsers returning typed values, an `ArgGroup`, `value_parser!(usize).range(1..)`, `NonZeroUsize`.

## PLT-027 - Default dataset/variant literals and "no dataset" spellings

Reported by: config-cli-bootstrap, pbfhogg-nidhogg, elivagar, small-benches.

`default_value = "denmark"` ~30× in `src/cli/schema.rs`; `"indexed"`/`"raw"` ~20×; `bootstrap.rs` hard-codes `"denmark"`/`"raw"` for `PmtilesWriter`/`NodeStore`; the default variant varies by command; nidhogg `RunApi`/`RunTiles` hard-code `"raw"` with no `--variant`. A host with no `denmark` gets a confusing resolution error. "No dataset" is `""` (dellingr, mogwai, piners), `"n/a"` (sluggrs hotpath), or `"denmark"` (synthetic benches); `MeasureRequest.dataset`/`variant` are passed but read by none of dellingr, mogwai, sluggrs. `hotpath.md` says rows carry `n/a`; those values never reach the DB.

Enforcement proposed: a per-host `default_dataset` or one const; `Option<&str>`; textlint on `default_value = "denmark"` in `src/cli/**`.

## PLT-028 - Exit codes have no table

Reported by: config-cli-bootstrap, check-core, elivagar, piners.

1, 3 (regress/corpus stale), 10 (partial), 124 (`WATCHDOG_EXIT_CODE`, the only named one), 130 (literal twice in `main`), 2/127 in `rustc_guard`; corpus 2 collides with clap's usage error (BUG-061); piners' harness 0/1/2 contract exists only as a `match` in `cmd.rs` and prose. Passthrough child codes are forwarded verbatim via `ExitCode(out.code)`, so a child exiting 10, 124 or 130 reads as partial, watchdog or interrupt. Check's verdict table: VAL-004.

Enforcement proposed: an `exit` module with named constants and remapping of child codes.

## PLT-029 - Platform tunables live where first needed, with no index

Reported by: config-cli-bootstrap, process-control.

`OUTPUT_RETENTION` (elivagar dispatch), `DEFAULT_KEEP` (worktree_record), `MIN_FILTER_LEN` (parser), `STATUS_WIDTH`, `DEADLINE_POLL_INTERVAL`, `SIGTERM_FORWARD_BUDGET` (output), `JDK_MAJOR`, `OSMOSIS_VERSION` (tools), `INDEX_MIN_SECTIONS` (man), `DEFAULT_THRESHOLD` (wc), `DRAIN_BUDGET`, `DRAIN_REAP_AFTER`, the 100ms drain poll, the 3s handshake probe, OOM score 1000, depth caps 256/64, `read_lock_contents`' 3×10ms retry. No `brokkr man` section or module lists them. Check/test: VAL-011; measurement: MEA-010; projects: PRJ-007, PRJ-024, PRJ-035, PRJ-046, PRJ-056.

## PLT-030 - External tools: unpinned, unverified, re-discovered per site

Reported by: config-cli-bootstrap, elivagar.

curl has three call shapes (`run_curl`, `download_file`, `head_url`) with no retry or checksum (and no timeout: BUG-017). JDK, planetiler, tilemaker and shortbread are "latest"/HEAD and unpinned, so comparison baselines drift silently; tilemaker does a network `git pull` every run. PATH lookup via `which` is re-implemented in `preflight::check_binary`, `tools::check_curl`, `tools::check_build_tool`, elivagar `check_unzip` ×2 and `check_ogr2ogr`. `tools::ensure_osmosis(workspace_root)` carries `#[allow(unused_variables)]`.

## PLT-031 - Assorted duplicated platform constants

Reported by: config-cli-bootstrap.

`rustflags-` prefix in `commands.rs` and `check_cmd/output.rs`; CPU fallback 4 in `tools.rs` and `cpu_topology.rs`; `/proc/version` parsing in `env.rs` and `history_cmd.rs`; io_uring kernel params in `env::check_uring_blocked` and `preflight::uring_checks` (env's doc claims "checks the same"; env never reports the 16 MB memlock bar); `wc`'s default 800 restated in its help doc comment and CLAUDE.md.

## PLT-032 - Build-path rules written per call site

Reported by: process-control.

The worktree `CARGO_TARGET_DIR` isolation rule is in `build::cargo_build_observed` and `bench_cmd::cargo_bench` only; `docs/commands/bench.md` calls it "the same rule every other brokkr build path applies". `build::project_info` for a worktree runs metadata without the override, so `ResolvedPaths.target_dir` names the shared target dir. `pbfhogg/bench_all.rs` hand-rolls `cargo build --release --message-format=json` + `find_executable` instead of `cargo_build`. `find_executable`'s doc says it "falls back to the last executable"; the code errors on more than one. `cfg!(windows)` in `resolve_existing_binary` is dead (Linux-only crate).

## PLT-033 - `cmd_install` duplicates `install_bin_targets`

Reported by: process-control.

It copies the logic and the error string ("in the same words"); the doc says they "cannot drift", but they agree only by duplication.

## PLT-034 - Lock context labels and roots are inconsistent

Reported by: process-control, pbfhogg-nidhogg, config-cli-bootstrap.

`bench_cmd` says `project_root` is "the project root everywhere else"; run, install, deps, clippy and fmt pass `build_root`, check passes `state_root`. `acquire_cmd_lock(..., "run")` is used for `passthrough`. pbfhogg labels bench mode `"run {id}"`; nidhogg uses `"bench nid-ingest"`, a hand-typed `"run nid-ingest"`, and `"hotpath …"`. Literal project names: PLT-024.

## PLT-035 - `RUN_VALUE_FLAGS` is maintained by hand

Reported by: process-control.

`["--features", "-F"]`; adding a value flag to `run` silently breaks the `--` pre-pass. Enforcement proposed: a test comparing against clap introspection (`Cli::command().find_subcommand("run")` args that take values). See BUG-031.

## PLT-036 - Guard verification scripts are claims, not gates

Reported by: process-control.

`scripts/guard-smoke.py` flocks the real `~/.brokkr/brokkr.lock` and is not wired into any `[[script_check]]`; neither is `guard-decision-probe.py`, which check.md says "is worth running after any change". The smoke's three cases are a subset of the probe's, so it looks superseded.

## PLT-037 - Secrets and personal data can reach durable logs

Reported by: config-cli-bootstrap, check-core.

`clippy --env KEY=VALUE`, passthrough args and similar land verbatim in the global `history.db` (`raw_args`) and in `brokkr_args`; `capture_env` guards `*` but nothing else is scrubbed. Captured script-check output is persisted verbatim in `.brokkr/check-logs`. History stores argv joined unquoted while `capture_brokkr_args` shell-quotes: two renderings of one argv (MEA-008).

## PLT-038 - Tests write to `/tmp` and use ad-hoc scratch allocators

Reported by: test-execution, measurement-storage, piners, config-cli-bootstrap, pbfhogg-nidhogg, elivagar.

`test_scratch.rs` calls itself "the one scratch-directory allocator" and claims every test writes under `target/test-tmp/`. Against that: ~20 tests use `std::env::temp_dir()` (i.e. `/tmp`, against project rules), some with fixed names and no pid - in `db/write.rs`, `db/query.rs`, `db/sidecar.rs`, `harness_mod/tests.rs`, `db/migrate.rs`, and eleven piners tests (`bless.rs`, `reseed.rs` ×4, `lfs.rs`, `registry.rs`, `lint/reseed.rs`, `lint/registry.rs`, `lint/db.rs`, which comments that this keeps "/tmp literal out of source"); `preflight` `tree_hash_tests` use `target/` + timestamp (leaked); `resolve_parts/tests.rs` uses `cwd/.brokkr/test-artifacts` (brokkr's own state dir, cwd-dependent); `download_modes.rs::run_rotation` uses `current_dir()/.brokkr/test-artifacts/rotation-<pid>-<nanos>` (leaks on panic); `preflight.rs` uses `CARGO_MANIFEST_DIR`; `elivagar/corpus/fixture.rs::TestDir` writes to `CARGO_MANIFEST_DIR/target` regardless of the target dir, keeps leftovers, and its per-process counter restarts under nextest.

Enforcement proposed: textlint bans on `temp_dir()`, `test-artifacts`, and `CARGO_MANIFEST_DIR` in `src/**/*.rs` outside `test_scratch.rs`.

## PLT-039 - Tests mutate process state or depend on the host

Reported by: config-cli-bootstrap, process-control, measurement-storage, ratatoskr, check-core, test-execution.

- Process env mutated in parallel test binaries: `config_parts/tests.rs::empty_env_override_disables_the_layer` (`BROKKR_USER_CONFIG`), `history.rs::db_path_uses_xdg_data_home` (`XDG_DATA_HOME`, with a "single-threaded" SAFETY comment that is false under libtest). Fix proposed: path resolution takes an env accessor.
- Host utilities: `output.rs` tests need `/bin/true`, `/bin/sleep`, `/bin/echo`; `preflight::check_binary`, `tools::check_curl`/`check_build_tool` need `which`; ratatoskr `process.rs` tests need `sleep` on PATH, 150ms wall windows, "kernel does not reuse PIDs immediately"; `write_artefacts_omits_git_keys_when_collection_fails` depends on host git and a gitlink trick.
- Host `/proc`: sidecar tests (`/proc/self/io` denied in some sandboxes), `stray::tests::own_process_tree_reads_without_panicking` (asserts nothing), watchdog tests; lockfile tests on the real `$HOME` (BUG-027).
- Check/test specifics: VAL-023.

## PLT-040 - Durable text cites transient IDs; the notes-citation rules are too narrow

Reported by: convention-engines, config-cli-bootstrap, ratatoskr, pbfhogg-nidhogg.

- CLAUDE.md applies no-line-numbers to "notes/, docs/ and reference/ alike", but `docs-cite-no-line-numbers` covers `docs/**` and only `.rs:N`; `notes/todo.md` cites `src/test_runner.rs:520` and `:21`.
- CLAUDE.md says nothing durable may cite notes/; `README.md` cites `notes/sidecar.md`; no rule covers README, CLAUDE.md or `scripts/`.
- `comments-never-cite-notes` keys on the literal `notes/` in comments only: ~40 comments cite transient IDs (`S3-06`…`S3-35`, `TODO #5`, "request 2", "plan 1/plan-3", "feature 6 `when`", "plan 2", "plan 3", "Phase 8", "Q5:", "the C3 refresh feature"); the `man/render.rs` S-IDs collide with mogwai's tracker namespace by the file's own admission; "(plan 3 follow-up)" appears in a user-facing error; `verify_renumber.rs` prints "notes/renumber-planet-scale.md" (a string literal); `smoke.js` cites `HARNESS-IMPROVEMENTS.md` (not in this repo); `saehrimnir.rs` header has an absolute `/home/folk/Programs/sæhrimnir/` path; several elivagar headers cite `brokkr.md`/`elivagar.md`, which don't exist here; `download_core.rs::rotate_dataset_to_snapshot` says a limitation is "documented in CLAUDE.md" (it isn't); `parse_check`'s error says "See CLAUDE.md for examples" (none there).

Enforcement proposed: widen `paths` of the three existing rules (README, CLAUDE.md, scripts, notes), extend to string regions, and add patterns for `\bplan \d|Phase \d`, `S\d-\d+` and `/home/`.

## PLT-041 - Docs and comments hard-code counts that drift

Reported by: process-control, ratatoskr, pbfhogg-nidhogg, config-cli-bootstrap, test-execution, check-core, piners, elivagar.

Per the user's documentation rule these should be reworded so no drifting specific is hard-coded, not re-counted: "~25 cargo call sites" (`hold.rs`, check.md); ratatoskr "14 zero-drift rules" (`run_gate_cohort` doc, a test comment) and `ratatoskr-gate.md` "14 of the 16 gates" (ratatoskr's config has 17 gates today; the two claims also disagree with each other); "11 commands + all" for pbfhogg verify (`docs/projects/pbfhogg.md`, CLAUDE.md; there are 12); "Pbfhogg measured commands: 28 commands" in `bootstrap.rs` (`as_pbfhogg` handles 17) and "26 commands" in `commands.rs` headers; "Eleven modules" in `test_scratch.rs`; check.md "a tenth phase"; "five-line sentinel" / "five" protocols in ratatoskr docs and comments (nine protocols); `Cargo.toml` `description` names three projects; README says `project` is one of 4 names; the 270s piners ceiling restated in three `cmd.rs` comments, CLI help (`~270s`, `--over 269`) and `corpus.md` twice; docs restating 50ms/10s/1.5s ratatoskr timing and 20s/120s drain values; the 2026-07-14 incident retold in five places.

## PLT-042 - Stale claims in CLI help, README, CLAUDE.md and config docs

Reported by: config-cli-bootstrap, piners, elivagar.

- README.md: documents a `preview` command, a `--wait` flag on "all commands" and a `[hostname.preview]` table (none exist; `HostConfig` is `deny_unknown_fields`, so the README's config example fails to load); `check` is "gremlin scan + dependency rules + clippy + tests"; `test` "always builds release" (`[test] debug` exists); cites `notes/sidecar.md`.
- CLI help: `check` long_about "Three phases in order: gremlin scan, clippy, then tests" and an enforced `--test-threads=1`; `man` help lists agnostic topics omitting `bench` and `clean`; `history` help hard-codes `~/.local/share/brokkr/history.db`; `History.id` claims ids are shown (BUG-034); `PmtilesCorpusCommand` docs (PRJ-029); piners help (PRJ-050).
- Config docs: `parse_test` doc claims detection of `consumer_features` inside `[test]` (code checks only `sweeps`); `resolve_pmtiles_by_commit` doc uses the old `<dataset>-<commit>` name; `env::EnvInfo` says "the `dev env` subcommand"; `docs/brokkr.toml.md` lists `port = 3033` with no meaning and never mentions `PORT` (BUG-088).
- CLAUDE.md: `DevError` list omits `ExitCode` and `Interrupted`; the `Project` enum list omits Brokkr, Saehrimnir, Dellingr, Mogwai, Other; tools.rs is said to do "osmium" discovery (it does osmosis, JDK, planetiler, tilemaker); the `piners-config` topic summary in `man.rs` and CLAUDE.md list a `feeds` key that the doc says does not exist.
- `man::TOPICS` covers every doc today, but no test enumerates `docs/**/*.md`, so a new doc silently won't appear in `man`.

## PLT-043 - Stale claims in process-control code and docs

Reported by: process-control.

- `rustc_guard.rs` header: "The `/proc` stat parse mirrors `src/stray.rs`" and "an unreadable `/proc`" as a fail-open case; the `auth_hash` doc says it is duplicated "in the same way the `/proc` parse was duplicated". The guard reads no `/proc` any more.
- `LockState.draining` doc: "Published for diagnostics only - `brokkr lock` and the wait message"; check.md says it lets `brokkr lock` and `kill` see who is draining. `parse_lock_contents` ignores it and `LockInfo` has no such field.
- Toolchain header: activation "immediately after taking the flock" (it's after the drain and capability publication); its list of hard kills omits brokkr's own watchdog exit (BUG-011).
- `worktree_record` header and `docs/brokkr.toml.md`: "brokkr caches each worktree's measured size" (`size_bytes` is never written).
- check.md: "admitted compilation holds a reader lease" reads broader than the code (the Idle decision drops the lease; only the stray reap covers such compiles).
- `oom.rs` "degrades on non-Linux" (the crate is Linux-only).
- Worktree retention: the per-project count is acknowledged as a damper only; `enforce` runs a `git status` per candidate on every `--commit` run.
- check.md restates `DRAIN_REAP_AFTER`/`DRAIN_BUDGET`, the stamp call-site list, and the cargo-family comm list owned by `stray::is_cargo_family`.

## PLT-044 - Misattached doc comments in config

Reported by: config-cli-bootstrap.

`resolve_features`' doc is glued onto `profile_override`; the "Cross-check that every sweep name…" doc sits on `parse_quarantine`; `profile_override`'s doc says ratatoskr-only but it serves `test`, `service`, `corpus`, `lint-corpus`. The per-profile doctest rule in `validate_complete_universe` counts `parallel.is_none()` as serial while the global rule counts `parallel || nextest` as unable to run doctests, so a complete profile whose non-curated sweeps are all nextest passes load with doctests never running - two implementations of one rule, already diverged.

## PLT-045 - Dead code in platform modules

Reported by: config-cli-bootstrap, process-control.

- `Project::is_builtin`: `#[allow(dead_code)]`, no callers (also noted by pbfhogg-nidhogg). `Project::Saehrimnir`/`Project::Brokkr` behave like `Other` except for an exhaustive match in `env.rs`.
- `output::history_msg` `#[allow(dead_code)]`.
- `preflight::Check::{File, DiskSpace, KernelParam}` ("not yet constructed by any caller").
- `Snapshot.osc` ("not consumed by any current command"); `history::run_migrations` empty placeholder.
- `passthrough` hidden and self-described "deprecated - use `run`".
- `#[allow(dead_code)]` on whole structs (`PbfEntry`, `Dataset`, `TilegenConfig`, `HostConfig`, `LitehtmlFixture`, `SluggrsSnapshot`, `ResolvedPaths`) hides which fields are unread.
- Compatibility aliases with nothing showing they still matter: `sha256` on `PbfEntry`/`OscEntry`/`PmtilesEntry` (and `xxhash` vs `Dataset.xxh128` as a second digest field name), `[clippy]` as a `[lints]` alias, the `[ratatoskr.harness].sweep` (its only consumer no longer has `sweep`), `[check]` table and `[test.sweeps]` refusals.
- `Record.size_bytes` (never written), `LockState.draining` (never read), `resolve_bin_name`'s `resolve.root` branch, `scripts/guard-smoke.py` (PLT-036).
- `HarnessConfig::binary_name` is `#[cfg(test)]`-only; its doc says it exists "to lock the defaulting rule into the schema", but production re-implements the rule, so the test pins a copy.
- `history::fresh_db_schema_version` cannot fail (`test_db()` writes `SCHEMA_VERSION` and the test reads it back); `test_db` duplicates `open()`'s setup, so `run_migrations`/`open` are never exercised.
- `hold::tests::stamp_is_a_no_op_without_a_capability` never inspects `cmd.get_envs()`.
