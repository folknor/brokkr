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

Residue: broader print/panic/indexing/expect denies and transient labels elsewhere in the tree remain.

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

Residue: direct `Command` construction outside the choke point remains in `test_runner/procgroup.rs`, `ratatoskr/saehrimnir.rs`, nidhogg server/bench/client, `env.rs`, `gremlins.rs`, `litehtml/cmd.rs`, `tools.rs`, `worktree.rs`, `scope.rs`, `git.rs`, `git/tree_state.rs`, the elivagar downloads and pbfhogg verify.

## PLT-008 - Errors swallowed into defaults (platform code)

Reported by: config-cli-bootstrap, process-control.

Residue: `history.rs` drops row decode errors, `preflight/digest.rs` ignores `cache.save()` results, and history metadata falls back to `"unknown"`.

## PLT-009 - Panics on input an operator or file controls

Reported by: elivagar, pbfhogg-nidhogg, piners, measurement-storage, ratatoskr.

Residue: production panic branches remain in `ratatoskr/sync/gate.rs` and `piners/measured.rs`.

## PLT-012 - Interrupt handling residue outside `SigtermGuard`

Reported by: process-control, ratatoskr.

Residue: ratatoskr's mock-serve `AtomicBool` signal guard is not migrated to the shared guard.

## PLT-013 - Hold-scoped state still partly in process globals

Reported by: process-control.

Residue: the toolchain path reaches fresh holds through a write-once process global in `context.rs` rather than being passed from detection, and `HELD` stays process-wide.

## PLT-015 - `cargo metadata` is spawned and deserialized many times

Reported by: convention-engines, check-core, process-control.

Residue: `nextest_lane.rs` still spawns metadata directly, `direct_runtime.rs`/`prepare.rs` reparse `raw_json`, and `dependency_rules.rs` keeps its own schema.

## PLT-016 - Git invocations and the tracked-file walk have many owners

Reported by: convention-engines, process-control, config-cli-bootstrap.

Residue: the convention scanners share one snapshot, but the prose-only shortcut still runs its own `git status` through `scope::dirt` rather than reading the snapshot.

## PLT-017 - "Dirty tree" has several definitions

Reported by: process-control, measurement-storage, small-benches.

Residue: `scope.rs` still has its own status reader, and `TreeState` still names the toolchain's `FILES`/`SUFFIX` itself.

## PLT-019 - Build profile is a bare string or bool

Reported by: process-control, ratatoskr, test-execution, piners.

Residue: profile resolution is still bool-based, and some DB writers record `Release` regardless of the built artifact.

## PLT-021 - `brokkr.toml` is parsed more than once per invocation

Reported by: config-cli-bootstrap, process-control.

Residue: `env.rs`, `bench_cmd` and `git::collect` still compose or resolve config independently of the startup detection.

## PLT-023 - "Which command runs in which project" has two owners

Reported by: config-cli-bootstrap, small-benches, elivagar.

Residue: nested or duplicate project checks remain in `verify_cmd.rs`, `visual_approve_cmd.rs`, `pmtiles_stats_cmd.rs` and project handlers.

## PLT-024 - Project name ↔ variant mapping is duplicated

Reported by: config-cli-bootstrap, ratatoskr, pbfhogg-nidhogg.

Residue: `Project::Other` still uses `Box::leak` in the parser.

## PLT-025 - Configuration is validated at use instead of at load

Reported by: config-cli-bootstrap, process-control.

Residue: project-specific string validation is still at use time across pbfhogg, elivagar, ratatoskr, piners and litehtml.

## PLT-026 - CLI values are parsed or checked twice

Reported by: config-cli-bootstrap, measurement-storage, pbfhogg-nidhogg.

Residue: validation for `--osc-range`, compression and some run counts is still duplicated.

## PLT-030 - External tools: unpinned, unverified, re-discovered per site

Reported by: config-cli-bootstrap, elivagar.

Residue: nidhogg's separate curl timeout contract and mutable map-data URLs remain.

## PLT-031 - Assorted duplicated platform constants

Reported by: config-cli-bootstrap.

Residue: the CPU fallback of 4 is still duplicated in `tools.rs` and `cpu_topology.rs`.

## PLT-032 - Build-path rules written per call site

Reported by: process-control.

Residue: `pbfhogg/bench_all.rs::run_osmpbf_baseline` bypasses the shared build path, and there is no `BuildRoot` cargo spawn API.

## PLT-036 - Guard verification scripts are claims, not gates

Reported by: process-control.

Residue: the guard decision probe has no script_check gate and writes to `/tmp` through `tempfile`.

## PLT-037 - Secrets and personal data can reach durable logs

Reported by: config-cli-bootstrap, check-core.

Residue: argv and `env.*` redaction in `db/write.rs`, raw script-check output rendering, and stored cwd/hostname remain.

## PLT-038 - Tests write to `/tmp` and use ad-hoc scratch allocators

Reported by: test-execution, measurement-storage, piners, config-cli-bootstrap, pbfhogg-nidhogg, elivagar.

Residue: ad-hoc temp paths in tests remain in `db/{write,query,sidecar,migrate}.rs`, `harness_mod/tests.rs`, piners tests, `resolve_parts/tests.rs`, `pbfhogg_mod/download_modes.rs` and `elivagar/corpus/fixture.rs`.

## PLT-039 - Tests mutate process state or depend on the host

Reported by: config-cli-bootstrap, process-control, measurement-storage, ratatoskr, check-core, test-execution.

Residue: `config_parts/tests.rs` still mutates `BROKKR_USER_CONFIG`, other host-dependent tests remain, and `tools::sha256_file` shells out to `sha256sum`/`shasum`.

## PLT-040 - Durable text cites transient IDs; the notes-citation rules are too narrow

Reported by: convention-engines, config-cli-bootstrap, ratatoskr, pbfhogg-nidhogg.

Residue: transient labels remain in `check_cmd/phase.rs`, `test_cmd.rs`, `osc.rs`, `resolve_parts/schema.rs` and `scripts/smoke-certifies.sh`.

## PLT-041 - Docs and comments hard-code counts that drift

Reported by: process-control, ratatoskr, pbfhogg-nidhogg, config-cli-bootstrap, test-execution, check-core, piners, elivagar.

Residue: the repeated `1.5s` comments in `ratatoskr/service/suite.rs` and `sync/bench.rs` remain.

## PLT-042 - Stale claims in CLI help, README, CLAUDE.md and config docs

Reported by: config-cli-bootstrap, piners, elivagar.

Residue: the `dev env` wording in `env.rs` remains.

## PLT-043 - Stale claims in process-control code and docs

Reported by: process-control.

Residue: `worktree_record.rs`'s size-cache description and `oom.rs`'s non-Linux wording are still stale.

## PLT-045 - Dead code in platform modules

Reported by: config-cli-bootstrap, process-control.

Residue: `Record.size_bytes` has no production writer, and `Project::is_builtin` and unused preflight variants remain.

## PLT-047 - Detection errors now fail commands that may previously have tolerated a broken `brokkr.toml`

Reported by: the wave-5 reviewer, laterally.

Residue: the widening was reverted because it let `check` run green on a malformed config; which optional-config commands may ignore a detection error needs a decision.

## PLT-049 - `env` composes the config a second time to read the user layer

Reported by: the wave-5 reviewer, laterally.

Residue: `env` still recomposes config instead of receiving `ConfigSources` from detection.

## PLT-050 - Smaller leftovers the wave-7 review surfaced

Reported by: the wave-7 reviewer, laterally.

Residue: nidhogg ingest planning can still write the hash cache, and S3 references remain in `test_cmd.rs`, `check_cmd/coverage.rs` and `check_cmd/phase.rs`.

## PLT-051 - Decisions and leftovers from the command-spec wave

Reported by: the wave-9 reviewer.

Residue: the stale no-lock comment in `verify_cmd.rs` and the phase-type, threaded-test wall, typed-root and dispatch-table cleanup remain.
