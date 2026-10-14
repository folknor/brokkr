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

## PLT-002 - brokkr enforces very little of its own convention on itself

Reported by: every hunter (check-core, process-control, config-cli-bootstrap, measurement-storage, pbfhogg-nidhogg, ratatoskr, piners, small-benches).

Residue: no broader print/panic/indexing/expect denies were added.

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

Residue: direct `Command` construction remains in `test_runner/procgroup.rs`, `ratatoskr/saehrimnir.rs`, nidhogg, `env.rs`, `gremlins.rs`, `litehtml/cmd.rs`, `tools.rs`, `worktree.rs`, `scope.rs`, `git.rs` and `git/tree_state.rs`.

## PLT-012 - Interrupt handling residue outside `SigtermGuard`

Reported by: process-control, ratatoskr.

Residue: ratatoskr's `MOCK_SIGNAL_FLAG` `AtomicBool` is not migrated to the shared guard.

## PLT-013 - Hold-scoped state still partly in process globals

Reported by: process-control.

Residue: the toolchain-path write-once global in `context.rs` and the process-wide `HELD` registry remain.

## PLT-015 - `cargo metadata` is spawned and deserialized many times

Reported by: convention-engines, check-core, process-control.

Residue: `nextest_lane.rs` still spawns metadata directly, `direct_runtime.rs`/`prepare.rs` reparse `raw_json`, and `dependency_rules.rs` keeps its own schema.

## PLT-016 - Git invocations and the tracked-file walk have many owners

Reported by: convention-engines, process-control, config-cli-bootstrap.

Residue: the prose-only shortcut still runs its own `git status` through `scope::dirt`.

## PLT-017 - "Dirty tree" has several definitions

Reported by: process-control, measurement-storage, small-benches.

Residue: `scope.rs` keeps its own status reader and `TreeState` still names the toolchain's `FILES`/`SUFFIX`.

## PLT-019 - Build profile is a bare string or bool

Reported by: process-control, ratatoskr, test-execution, piners.

Residue: harness profile resolution is still bool-based at the ratatoskr/piners callers, and the DB writers that record `Release` regardless are unchanged.

## PLT-021 - `brokkr.toml` is parsed more than once per invocation

Reported by: config-cli-bootstrap, process-control.

Residue: `env.rs`, `bench_cmd` and `git::collect` still compose or resolve config independently of startup detection.

## PLT-023 - "Which command runs in which project" has two owners

Reported by: config-cli-bootstrap, small-benches, elivagar.

Residue: duplicate project checks remain in `verify_cmd.rs`, `visual_approve_cmd.rs`, `pmtiles_stats_cmd.rs` and project handlers.

## PLT-024 - Project name ↔ variant mapping is duplicated

Reported by: config-cli-bootstrap, ratatoskr, pbfhogg-nidhogg.

Residue: `Project::Other` still uses `Box::leak` in the parser.

## PLT-025 - Configuration is validated at use instead of at load

Reported by: config-cli-bootstrap, process-control.

Residue: project-specific string validation is still at use time.

## PLT-026 - CLI values are parsed or checked twice

Reported by: config-cli-bootstrap, measurement-storage, pbfhogg-nidhogg.

Residue: `--osc-range`, compression and run-count validation are still duplicated.

## PLT-030 - External tools: unpinned, unverified, re-discovered per site

Reported by: config-cli-bootstrap, elivagar.

Residue: nidhogg's separate curl timeout contract and mutable map-data URLs remain.

## PLT-036 - Guard verification scripts are claims, not gates

Reported by: process-control.

Residue: the guard decision probe still has no `[[script_check]]` gate (its `/tmp` use is fixed).

## PLT-037 - Secrets and personal data can reach durable logs

Reported by: config-cli-bootstrap, check-core.

Residue: argv and `env.*` redaction in `db/write.rs` and raw script-check output rendering remain (stored cwd/hostname are gone).

## PLT-038 - Tests write to `/tmp` and use ad-hoc scratch allocators

Reported by: test-execution, measurement-storage, piners, config-cli-bootstrap, pbfhogg-nidhogg, elivagar.

Residue: `std::env::temp_dir()` test paths remain in `db/{write,query,sidecar,migrate}.rs` and `harness_mod/tests.rs`, plus ad-hoc paths in `resolve_parts/tests.rs`.

## PLT-039 - Tests mutate process state or depend on the host

Reported by: config-cli-bootstrap, process-control, measurement-storage, ratatoskr, check-core, test-execution.

Residue: host-dependent tests remain (`resolve_parts/tests.rs` builds paths from `current_dir` and the clock).

## PLT-049 - `env` composes the config a second time to read the user layer

Reported by: the wave-5 reviewer, laterally.

Residue: `env` still recomposes config instead of receiving `ConfigSources` from detection.

## PLT-051 - Decisions and leftovers from the command-spec wave

Reported by: the wave-9 reviewer.

Residue: the phase-type, threaded-test-wall, typed-root and dispatch-table cleanup remain (the `verify_cmd.rs` comment is fixed).
